use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, bail};
use eyes_on_me_shared::{
    ActivityApp, ActivityEvent, ActivityKind, ActivitySearchHit, DashboardSnapshot, DeviceStatus,
    Platform, PresenceState,
};
use sqlx::{
    ConnectOptions, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use time::OffsetDateTime;

pub async fn connect(database_url: &str) -> anyhow::Result<SqlitePool> {
    let sqlite_path = database_url.trim_start_matches("sqlite://");
    if !sqlite_path.is_empty() && sqlite_path != ":memory:" {
        ensure_parent_dir(sqlite_path).await?;
        migrate_legacy_database_file(sqlite_path)?;
    }

    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .disable_statement_logging();

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .with_context(|| format!("failed to connect to database at {database_url}"))?;

    migrate(&pool).await?;
    Ok(pool)
}

fn migrate_legacy_database_file(target_path: &str) -> anyhow::Result<()> {
    let target = PathBuf::from(target_path);
    if target.exists() {
        return Ok(());
    }

    let parent = match target.parent() {
        Some(parent) => parent,
        None => return Ok(()),
    };

    let legacy_candidates = [
        parent.join("amiokay.db"),
        parent.join("../data/amiokay.db"),
        parent.join("../DB/amiokay.db"),
    ];

    if let Some(source) = legacy_candidates.into_iter().find(|path| path.exists()) {
        fs::copy(&source, &target).with_context(|| {
            format!(
                "failed to migrate legacy sqlite database from {} to {}",
                source.display(),
                target.display()
            )
        })?;
    }

    Ok(())
}

pub async fn load_snapshot(pool: &SqlitePool) -> anyhow::Result<DashboardSnapshot> {
    let recent_rows = sqlx::query(
        r#"
        SELECT event_id, ts, device_id, agent_name, platform, kind, app_json, window_title, browser_json, presence, source
        FROM activity_log
        WHERE kind != 'activity_sample'
        ORDER BY ts DESC
        LIMIT 20
        "#,
    )
    .fetch_all(pool)
    .await?;

    let recent_activities = recent_rows
        .iter()
        .map(activity_from_row)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let device_rows = sqlx::query(
        r#"
        SELECT event_id, ts, device_id, agent_name, platform, kind, app_json, window_title, browser_json, presence, source
        FROM (
            SELECT *,
                   ROW_NUMBER() OVER (PARTITION BY device_id ORDER BY ts DESC) AS row_num
            FROM activity_log
        )
        WHERE row_num = 1
        ORDER BY ts DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    let devices = device_rows
        .iter()
        .map(activity_from_row)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let latest_status = sqlx::query(
        r#"
        SELECT ts, device_id, agent_name, platform, status_text, source
        FROM device_status
        ORDER BY ts DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await?
    .map(|row| status_from_row(&row))
    .transpose()?;

    Ok(DashboardSnapshot {
        devices,
        latest_status,
        recent_activities,
    })
}

pub async fn persist_activity(pool: &SqlitePool, event: &ActivityEvent) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO activity_log (
            event_id, ts, device_id, agent_name, platform, kind, app_json, window_title, browser_json, presence, source
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ON CONFLICT(event_id) DO UPDATE SET
            ts = excluded.ts,
            device_id = excluded.device_id,
            agent_name = excluded.agent_name,
            platform = excluded.platform,
            kind = excluded.kind,
            app_json = excluded.app_json,
            window_title = excluded.window_title,
            browser_json = excluded.browser_json,
            presence = excluded.presence,
            source = excluded.source
        "#,
    )
    .bind(&event.event_id)
    .bind(event.ts.format(&time::format_description::well_known::Rfc3339)?)
    .bind(&event.device_id)
    .bind(&event.agent_name)
    .bind(platform_to_str(&event.platform))
    .bind(kind_to_str(&event.kind))
    .bind(serde_json::to_string(&event.app)?)
    .bind(&event.window_title)
    .bind(event.browser.as_ref().map(serde_json::to_string).transpose()?)
    .bind(presence_to_str(event.presence))
    .bind(&event.source)
    .execute(pool)
    .await?;

    upsert_activity_search(pool, event).await?;

    Ok(())
}

pub async fn persist_status(pool: &SqlitePool, status: &DeviceStatus) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO device_status (
            device_id, ts, agent_name, platform, status_text, source
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(device_id) DO UPDATE SET
            ts = excluded.ts,
            agent_name = excluded.agent_name,
            platform = excluded.platform,
            status_text = excluded.status_text,
            source = excluded.source
        "#,
    )
    .bind(&status.device_id)
    .bind(
        status
            .ts
            .format(&time::format_description::well_known::Rfc3339)?,
    )
    .bind(&status.agent_name)
    .bind(platform_to_str(&status.platform))
    .bind(&status.status_text)
    .bind(&status.source)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn load_device_statuses(pool: &SqlitePool) -> anyhow::Result<Vec<DeviceStatus>> {
    let rows = sqlx::query(
        r#"
        SELECT ts, device_id, agent_name, platform, status_text, source
        FROM device_status
        ORDER BY ts DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.iter().map(status_from_row).collect()
}

pub async fn load_device_status(
    pool: &SqlitePool,
    device_id: &str,
) -> anyhow::Result<Option<DeviceStatus>> {
    sqlx::query(
        r#"
        SELECT ts, device_id, agent_name, platform, status_text, source
        FROM device_status
        WHERE device_id = ?1
        LIMIT 1
        "#,
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?
    .map(|row| status_from_row(&row))
    .transpose()
}

pub async fn load_latest_activity_for_device(
    pool: &SqlitePool,
    device_id: &str,
) -> anyhow::Result<Option<ActivityEvent>> {
    sqlx::query(
        r#"
        SELECT event_id, ts, device_id, agent_name, platform, kind, app_json, window_title, browser_json, presence, source
        FROM activity_log
        WHERE device_id = ?1
        ORDER BY ts DESC
        LIMIT 1
        "#,
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?
    .map(|row| activity_from_row(&row))
    .transpose()
}

pub async fn load_recent_activities_for_device(
    pool: &SqlitePool,
    device_id: &str,
    limit: i64,
) -> anyhow::Result<Vec<ActivityEvent>> {
    let rows = sqlx::query(
        r#"
        SELECT event_id, ts, device_id, agent_name, platform, kind, app_json, window_title, browser_json, presence, source
        FROM activity_log
        WHERE device_id = ?1
          AND kind != 'activity_sample'
        ORDER BY ts DESC
        LIMIT ?2
        "#,
    )
    .bind(device_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.iter().map(activity_from_row).collect()
}

pub async fn load_all_activities_for_device(
    pool: &SqlitePool,
    device_id: &str,
) -> anyhow::Result<Vec<ActivityEvent>> {
    let rows = sqlx::query(
        r#"
        SELECT event_id, ts, device_id, agent_name, platform, kind, app_json, window_title, browser_json, presence, source
        FROM activity_log
        WHERE device_id = ?1
        ORDER BY ts ASC
        "#,
    )
    .bind(device_id)
    .fetch_all(pool)
    .await?;

    rows.iter().map(activity_from_row).collect()
}

pub async fn search_activities(
    pool: &SqlitePool,
    query: &str,
    device_id: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<ActivitySearchHit>> {
    let fts_query = build_fts_query(query)?;
    let rows = sqlx::query(
        r#"
        SELECT
            l.event_id,
            l.ts,
            l.device_id,
            l.agent_name,
            l.platform,
            l.kind,
            l.app_json,
            l.window_title,
            l.browser_json,
            l.presence,
            l.source,
            snippet(activity_search_fts, -1, '', '', ' … ', 12) AS snippet,
            bm25(activity_search_fts) AS score
        FROM activity_search_fts
        JOIN activity_search ON activity_search.rowid = activity_search_fts.rowid
        JOIN activity_log l ON l.event_id = activity_search.event_id
        WHERE activity_search_fts MATCH ?1
          AND (?2 IS NULL OR activity_search.device_id = ?2)
        ORDER BY bm25(activity_search_fts), l.ts DESC
        LIMIT ?3
        "#,
    )
    .bind(&fts_query)
    .bind(device_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(ActivitySearchHit {
                activity: activity_from_row(&row)?,
                snippet: row.try_get("snippet")?,
                score: row.try_get::<f64, _>("score").unwrap_or_default() as f32,
            })
        })
        .collect()
}

pub async fn count_activity_search_results(
    pool: &SqlitePool,
    query: &str,
    device_id: Option<&str>,
) -> anyhow::Result<i64> {
    let fts_query = build_fts_query(query)?;
    sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM activity_search_fts
        JOIN activity_search ON activity_search.rowid = activity_search_fts.rowid
        WHERE activity_search_fts MATCH ?1
          AND (?2 IS NULL OR activity_search.device_id = ?2)
        "#,
    )
    .bind(&fts_query)
    .bind(device_id)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS activity_log (
            event_id TEXT PRIMARY KEY,
            ts TEXT NOT NULL,
            device_id TEXT NOT NULL,
            agent_name TEXT NOT NULL,
            platform TEXT NOT NULL,
            kind TEXT NOT NULL,
            app_json TEXT NOT NULL,
            window_title TEXT,
            browser_json TEXT,
            presence TEXT NOT NULL DEFAULT 'active',
            source TEXT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        ALTER TABLE activity_log ADD COLUMN presence TEXT NOT NULL DEFAULT 'active'
        "#,
    )
    .execute(pool)
    .await
    .ok();

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_activity_log_device_ts
        ON activity_log(device_id, ts DESC)
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS device_status (
            device_id TEXT PRIMARY KEY,
            ts TEXT NOT NULL,
            agent_name TEXT NOT NULL,
            platform TEXT NOT NULL,
            status_text TEXT NOT NULL,
            source TEXT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS activity_search (
            event_id TEXT PRIMARY KEY,
            device_id TEXT NOT NULL,
            app_name TEXT NOT NULL,
            window_title TEXT,
            page_title TEXT,
            browser_url TEXT,
            browser_domain TEXT
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_activity_search_device_id
        ON activity_search(device_id)
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS activity_search_fts USING fts5(
            app_name,
            window_title,
            page_title,
            browser_url,
            browser_domain,
            content='activity_search',
            content_rowid='rowid',
            tokenize='unicode61 remove_diacritics 2'
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TRIGGER IF NOT EXISTS activity_search_ai
        AFTER INSERT ON activity_search
        BEGIN
            INSERT INTO activity_search_fts(
                rowid,
                app_name,
                window_title,
                page_title,
                browser_url,
                browser_domain
            )
            VALUES (
                new.rowid,
                new.app_name,
                new.window_title,
                new.page_title,
                new.browser_url,
                new.browser_domain
            );
        END
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TRIGGER IF NOT EXISTS activity_search_ad
        AFTER DELETE ON activity_search
        BEGIN
            INSERT INTO activity_search_fts(
                activity_search_fts,
                rowid,
                app_name,
                window_title,
                page_title,
                browser_url,
                browser_domain
            )
            VALUES (
                'delete',
                old.rowid,
                old.app_name,
                old.window_title,
                old.page_title,
                old.browser_url,
                old.browser_domain
            );
        END
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TRIGGER IF NOT EXISTS activity_search_au
        AFTER UPDATE ON activity_search
        BEGIN
            INSERT INTO activity_search_fts(
                activity_search_fts,
                rowid,
                app_name,
                window_title,
                page_title,
                browser_url,
                browser_domain
            )
            VALUES (
                'delete',
                old.rowid,
                old.app_name,
                old.window_title,
                old.page_title,
                old.browser_url,
                old.browser_domain
            );
            INSERT INTO activity_search_fts(
                rowid,
                app_name,
                window_title,
                page_title,
                browser_url,
                browser_domain
            )
            VALUES (
                new.rowid,
                new.app_name,
                new.window_title,
                new.page_title,
                new.browser_url,
                new.browser_domain
            );
        END
        "#,
    )
    .execute(pool)
    .await?;

    rebuild_activity_search_index_if_needed(pool).await?;

    Ok(())
}

async fn upsert_activity_search(pool: &SqlitePool, event: &ActivityEvent) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO activity_search (
            event_id,
            device_id,
            app_name,
            window_title,
            page_title,
            browser_url,
            browser_domain
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(event_id) DO UPDATE SET
            device_id = excluded.device_id,
            app_name = excluded.app_name,
            window_title = excluded.window_title,
            page_title = excluded.page_title,
            browser_url = excluded.browser_url,
            browser_domain = excluded.browser_domain
        "#,
    )
    .bind(&event.event_id)
    .bind(&event.device_id)
    .bind(&event.app.name)
    .bind(&event.window_title)
    .bind(
        event
            .browser
            .as_ref()
            .and_then(|browser| browser.page_title.clone()),
    )
    .bind(
        event
            .browser
            .as_ref()
            .and_then(|browser| browser.url.clone()),
    )
    .bind(
        event
            .browser
            .as_ref()
            .and_then(|browser| browser.domain.clone()),
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn rebuild_activity_search_index_if_needed(pool: &SqlitePool) -> anyhow::Result<()> {
    let activity_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activity_log")
        .fetch_one(pool)
        .await?;
    let search_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activity_search")
        .fetch_one(pool)
        .await?;

    if activity_count == search_count {
        return Ok(());
    }

    sqlx::query("DELETE FROM activity_search")
        .execute(pool)
        .await?;

    let rows = sqlx::query(
        r#"
        SELECT event_id, ts, device_id, agent_name, platform, kind, app_json, window_title, browser_json, presence, source
        FROM activity_log
        ORDER BY ts ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    for row in rows {
        let event = activity_from_row(&row)?;
        upsert_activity_search(pool, &event).await?;
    }

    sqlx::query("INSERT INTO activity_search_fts(activity_search_fts) VALUES('rebuild')")
        .execute(pool)
        .await?;

    Ok(())
}

fn build_fts_query(query: &str) -> anyhow::Result<String> {
    let tokens = query
        .split_whitespace()
        .map(|token| token.trim().trim_matches('"'))
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        bail!("search query is required");
    }

    Ok(tokens.join(" AND "))
}

fn activity_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<ActivityEvent> {
    Ok(ActivityEvent {
        event_id: row.try_get("event_id")?,
        ts: OffsetDateTime::parse(
            &row.try_get::<String, _>("ts")?,
            &time::format_description::well_known::Rfc3339,
        )?,
        device_id: row.try_get("device_id")?,
        agent_name: row.try_get("agent_name")?,
        platform: platform_from_str(&row.try_get::<String, _>("platform")?),
        kind: kind_from_str(&row.try_get::<String, _>("kind")?),
        app: serde_json::from_str::<ActivityApp>(&row.try_get::<String, _>("app_json")?)?,
        window_title: row.try_get("window_title")?,
        browser: parse_optional_json(row.try_get::<Option<String>, _>("browser_json")?)?,
        presence: presence_from_str(&row.try_get::<String, _>("presence")?),
        source: row.try_get("source")?,
    })
}

fn status_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<DeviceStatus> {
    Ok(DeviceStatus {
        ts: OffsetDateTime::parse(
            &row.try_get::<String, _>("ts")?,
            &time::format_description::well_known::Rfc3339,
        )?,
        device_id: row.try_get("device_id")?,
        agent_name: row.try_get("agent_name")?,
        platform: platform_from_str(&row.try_get::<String, _>("platform")?),
        status_text: row.try_get("status_text")?,
        source: row.try_get("source")?,
    })
}

fn platform_to_str(platform: &Platform) -> &'static str {
    match platform {
        Platform::Macos => "macos",
        Platform::Windows => "windows",
        Platform::Linux => "linux",
        Platform::Android => "android",
        Platform::Unknown => "unknown",
    }
}

fn platform_from_str(value: &str) -> Platform {
    match value {
        "macos" => Platform::Macos,
        "windows" => Platform::Windows,
        "linux" => Platform::Linux,
        "android" => Platform::Android,
        _ => Platform::Unknown,
    }
}

fn kind_to_str(kind: &ActivityKind) -> &'static str {
    match kind {
        ActivityKind::ForegroundChanged => "foreground_changed",
        ActivityKind::ActivitySample => "activity_sample",
        ActivityKind::PresenceChanged => "presence_changed",
    }
}

fn kind_from_str(value: &str) -> ActivityKind {
    match value {
        "foreground_changed" => ActivityKind::ForegroundChanged,
        "activity_sample" => ActivityKind::ActivitySample,
        "presence_changed" => ActivityKind::PresenceChanged,
        _ => ActivityKind::ForegroundChanged,
    }
}

fn presence_to_str(value: PresenceState) -> &'static str {
    match value {
        PresenceState::Active => "active",
        PresenceState::Idle => "idle",
        PresenceState::Locked => "locked",
    }
}

fn presence_from_str(value: &str) -> PresenceState {
    match value {
        "idle" => PresenceState::Idle,
        "locked" => PresenceState::Locked,
        _ => PresenceState::Active,
    }
}

fn parse_optional_json<T>(value: Option<String>) -> anyhow::Result<Option<T>>
where
    T: serde::de::DeserializeOwned,
{
    value
        .map(|raw| serde_json::from_str::<T>(&raw).map_err(anyhow::Error::from))
        .transpose()
}

async fn ensure_parent_dir(path: &str) -> anyhow::Result<()> {
    let file_path = Path::new(path);
    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use eyes_on_me_shared::{ActivityKind, BrowserContext, Platform};
    use time::OffsetDateTime;

    use super::{connect, count_activity_search_results, persist_activity, search_activities};

    fn sample_event(
        event_id: &str,
        device_id: &str,
        title: &str,
        url: Option<&str>,
    ) -> eyes_on_me_shared::ActivityEvent {
        eyes_on_me_shared::ActivityEvent {
            event_id: event_id.to_string(),
            ts: OffsetDateTime::now_utc(),
            device_id: device_id.to_string(),
            agent_name: "client-desktop".to_string(),
            platform: Platform::Macos,
            kind: ActivityKind::ForegroundChanged,
            app: eyes_on_me_shared::ActivityApp {
                id: "com.google.Chrome".to_string(),
                name: "Google Chrome".to_string(),
                title: Some(title.to_string()),
                pid: Some(42),
            },
            window_title: Some(title.to_string()),
            browser: url.map(|url| BrowserContext {
                family: "chromium".to_string(),
                name: "Google Chrome".to_string(),
                page_title: Some(title.to_string()),
                url: Some(url.to_string()),
                domain: Some("github.com".to_string()),
                source: "test".to_string(),
                confidence: 0.9,
            }),
            presence: eyes_on_me_shared::PresenceState::Active,
            source: "desktop".to_string(),
        }
    }

    #[tokio::test]
    async fn searches_window_titles_and_urls() {
        let pool = connect("sqlite::memory:")
            .await
            .expect("connect in-memory db");
        persist_activity(
            &pool,
            &sample_event(
                "evt-1",
                "mac-1",
                "GitHub Pull Request Review",
                Some("https://github.com/wm94i/Work_Review/pull/10"),
            ),
        )
        .await
        .expect("persist search event");

        let results = search_activities(&pool, "github review", None, 10)
            .await
            .expect("search activities");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].activity.event_id, "evt-1");
    }

    #[tokio::test]
    async fn filters_search_results_by_device() {
        let pool = connect("sqlite::memory:")
            .await
            .expect("connect in-memory db");
        persist_activity(
            &pool,
            &sample_event(
                "evt-1",
                "mac-1",
                "Rust RFC",
                Some("https://github.com/rust-lang/rfcs"),
            ),
        )
        .await
        .expect("persist first event");
        persist_activity(
            &pool,
            &sample_event(
                "evt-2",
                "mac-2",
                "Rust RFC",
                Some("https://github.com/rust-lang/rfcs"),
            ),
        )
        .await
        .expect("persist second event");

        let total = count_activity_search_results(&pool, "rust", Some("mac-2"))
            .await
            .expect("count filtered search");
        let results = search_activities(&pool, "rust", Some("mac-2"), 10)
            .await
            .expect("search filtered activities");

        assert_eq!(total, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].activity.device_id, "mac-2");
    }
}
