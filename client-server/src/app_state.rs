use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, RwLock},
};

use eyes_on_me_shared::{
    ActivityEvent, ActivityKind, ActivitySearchResponse, AnalysisOverviewResponse,
    BrowserUsageBucket, DashboardSnapshot, DeviceAnalysisResponse, DeviceAnalysisSummary,
    DeviceDetailResponse, DeviceOverview, DeviceStatus, DevicesResponse, DomainUsageBucket,
    PageUsageBucket, PresenceState, StreamMessage, UsageBucket,
};
use sqlx::SqlitePool;
use time::{Duration as TimeDuration, OffsetDateTime, UtcOffset};
use tokio::sync::broadcast;

const RECENT_ACTIVITY_LIMIT: usize = 20;
const DEVICE_ACTIVITY_LIMIT: i64 = 50;
const ANALYSIS_TOP_LIMIT: usize = 12;
const MAX_ACTIVITY_CONTINUITY: TimeDuration = TimeDuration::seconds(120);
const WORKDAY_START_HOUR: i64 = 9;
const WORKDAY_END_HOUR: i64 = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisRange {
    Last3Hours,
    Last6Hours,
    Today,
    LastDay,
    LastWeek,
    LastMonth,
    All,
}

impl AnalysisRange {
    pub fn from_query(value: Option<&str>) -> Option<Self> {
        match value.unwrap_or("today") {
            "3h" => Some(Self::Last3Hours),
            "6h" => Some(Self::Last6Hours),
            "today" => Some(Self::Today),
            "1d" => Some(Self::LastDay),
            "1w" => Some(Self::LastWeek),
            "1m" => Some(Self::LastMonth),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn window_start(self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.window_start_with_offset(now, UtcOffset::current_local_offset().ok())
    }

    fn window_start_with_offset(
        self,
        now: OffsetDateTime,
        local_offset: Option<UtcOffset>,
    ) -> Option<OffsetDateTime> {
        match self {
            Self::Last3Hours => Some(now - TimeDuration::hours(3)),
            Self::Last6Hours => Some(now - TimeDuration::hours(6)),
            Self::Today => Some(start_of_local_day(
                now,
                local_offset.unwrap_or(UtcOffset::UTC),
            )),
            Self::LastDay => Some(now - TimeDuration::days(1)),
            Self::LastWeek => Some(now - TimeDuration::days(7)),
            Self::LastMonth => Some(now - TimeDuration::days(30)),
            Self::All => None,
        }
    }
}

fn start_of_local_day(now: OffsetDateTime, local_offset: UtcOffset) -> OffsetDateTime {
    now.to_offset(local_offset)
        .date()
        .midnight()
        .assume_offset(local_offset)
}

#[derive(Debug)]
struct SnapshotState {
    snapshot: DashboardSnapshot,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<SnapshotState>>,
    pool: SqlitePool,
    tx: broadcast::Sender<StreamMessage>,
}

impl AppState {
    pub fn new(snapshot: DashboardSnapshot, pool: SqlitePool) -> Self {
        let (tx, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(RwLock::new(SnapshotState { snapshot })),
            pool,
            tx,
        }
    }

    pub fn snapshot(&self) -> DashboardSnapshot {
        self.inner
            .read()
            .expect("snapshot lock poisoned")
            .snapshot
            .clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamMessage> {
        self.tx.subscribe()
    }

    pub async fn upsert_activity(&self, event: ActivityEvent) -> anyhow::Result<()> {
        crate::db::persist_activity(&self.pool, &event).await?;
        let derived_status = derive_status_from_activity(&event);
        crate::db::persist_status(&self.pool, &derived_status).await?;
        let snapshot = {
            let mut guard = self.inner.write().expect("snapshot lock poisoned");
            let devices = upsert_device(&guard.snapshot.devices, &event);
            let recent_activities = push_recent_activity(&guard.snapshot.recent_activities, event);

            guard.snapshot.devices = devices;
            guard.snapshot.recent_activities = recent_activities;
            guard.snapshot.latest_status = Some(derived_status);
            guard.snapshot.clone()
        };

        self.broadcast_snapshot(snapshot);
        Ok(())
    }

    pub async fn update_status(&self, status: DeviceStatus) -> anyhow::Result<()> {
        crate::db::persist_status(&self.pool, &status).await?;
        let snapshot = {
            let mut guard = self.inner.write().expect("snapshot lock poisoned");
            guard.snapshot.latest_status = Some(status);
            guard.snapshot.clone()
        };

        self.broadcast_snapshot(snapshot);
        Ok(())
    }

    pub async fn devices_response(&self) -> anyhow::Result<DevicesResponse> {
        let snapshot = self.snapshot();
        let status_by_device = crate::db::load_device_statuses(&self.pool)
            .await?
            .into_iter()
            .map(|status| (status.device_id.clone(), status))
            .collect::<HashMap<_, _>>();

        let devices = snapshot
            .devices
            .into_iter()
            .map(|device| DeviceOverview {
                latest_status: status_by_device.get(&device.device_id).cloned(),
                device,
            })
            .collect();

        Ok(DevicesResponse { devices })
    }

    pub async fn device_detail(
        &self,
        device_id: &str,
    ) -> anyhow::Result<Option<DeviceDetailResponse>> {
        let current_device = self
            .snapshot()
            .devices
            .into_iter()
            .find(|device| device.device_id == device_id)
            .or(crate::db::load_latest_activity_for_device(&self.pool, device_id).await?);

        let Some(device) = current_device else {
            return Ok(None);
        };

        let latest_status = crate::db::load_device_status(&self.pool, device_id).await?;
        let recent_activities = crate::db::load_recent_activities_for_device(
            &self.pool,
            device_id,
            DEVICE_ACTIVITY_LIMIT,
        )
        .await?;

        Ok(Some(DeviceDetailResponse {
            device,
            latest_status,
            recent_activities,
        }))
    }

    pub async fn analysis_overview(
        &self,
        range: AnalysisRange,
    ) -> anyhow::Result<AnalysisOverviewResponse> {
        let now = OffsetDateTime::now_utc();
        let snapshot = self.snapshot();
        let latest_statuses = crate::db::load_device_statuses(&self.pool)
            .await?
            .into_iter()
            .map(|status| (status.device_id.clone(), status))
            .collect::<HashMap<_, _>>();

        let mut devices = Vec::new();
        let mut app_usage = HashMap::new();
        let mut domain_usage = HashMap::new();
        let mut browser_usage = HashMap::new();
        let mut total_tracked_ms = 0_u64;
        let mut work_tracked_ms = 0_u64;
        let mut browser_tracked_ms = 0_u64;
        let mut app_keys = HashSet::new();

        for current_device in snapshot.devices {
            let activities =
                crate::db::load_all_activities_for_device(&self.pool, &current_device.device_id)
                    .await?;
            if activities.is_empty() {
                continue;
            }

            let analysis = build_device_analysis_payload(
                current_device.device_id.clone(),
                latest_statuses.get(&current_device.device_id).cloned(),
                activities,
                now,
                range,
            );

            if analysis.total_tracked_ms == 0 {
                continue;
            }

            total_tracked_ms += analysis.total_tracked_ms;
            work_tracked_ms += analysis.work_tracked_ms;
            browser_tracked_ms += analysis.browser_tracked_ms;
            merge_usage_vec(&mut app_usage, &analysis.app_usage);
            merge_usage_vec(&mut domain_usage, &analysis.domain_usage);
            merge_browser_usage_vec(&mut browser_usage, &analysis.browser_usage);
            app_keys.extend(analysis.app_usage.iter().map(|bucket| bucket.key.clone()));

            devices.push(DeviceAnalysisSummary {
                device_id: current_device.device_id.clone(),
                platform: current_device.platform.clone(),
                current_label: analysis
                    .current_label
                    .clone()
                    .unwrap_or_else(|| current_activity_label(&current_device)),
                latest_status_text: analysis
                    .latest_status
                    .as_ref()
                    .map(|status| status.status_text.clone()),
                total_tracked_ms: analysis.total_tracked_ms,
                event_count: analysis.event_count,
                last_seen: current_device.ts,
            });
        }

        devices.sort_by(|a, b| {
            b.total_tracked_ms
                .cmp(&a.total_tracked_ms)
                .then_with(|| b.last_seen.cmp(&a.last_seen))
        });

        Ok(AnalysisOverviewResponse {
            generated_at: now,
            device_count: devices.len(),
            total_tracked_ms,
            work_tracked_ms,
            browser_tracked_ms,
            app_count: app_keys.len(),
            devices,
            top_app_usage: finalize_usage_map(app_usage),
            top_domain_usage: finalize_usage_map(domain_usage),
            top_browser_usage: finalize_browser_usage_map(browser_usage),
        })
    }

    pub async fn device_analysis(
        &self,
        device_id: &str,
        range: AnalysisRange,
    ) -> anyhow::Result<Option<DeviceAnalysisResponse>> {
        let activities = crate::db::load_all_activities_for_device(&self.pool, device_id).await?;
        if activities.is_empty() {
            return Ok(None);
        }

        let latest_status = crate::db::load_device_status(&self.pool, device_id).await?;
        Ok(Some(build_device_analysis_payload(
            device_id.to_string(),
            latest_status,
            activities,
            OffsetDateTime::now_utc(),
            range,
        )))
    }

    pub async fn search_activities(
        &self,
        query: &str,
        device_id: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<ActivitySearchResponse> {
        let results = crate::db::search_activities(&self.pool, query, device_id, limit).await?;
        let total = crate::db::count_activity_search_results(&self.pool, query, device_id).await?;

        Ok(ActivitySearchResponse {
            query: query.to_string(),
            device_id: device_id.map(str::to_string),
            total: total.max(0) as usize,
            results,
        })
    }

    fn broadcast_snapshot(&self, snapshot: DashboardSnapshot) {
        let _ = self.tx.send(StreamMessage::Snapshot(snapshot));
    }
}

fn upsert_device(devices: &[ActivityEvent], event: &ActivityEvent) -> Vec<ActivityEvent> {
    let mut next = devices.to_vec();

    if let Some(existing) = next
        .iter_mut()
        .find(|item| item.device_id == event.device_id)
    {
        *existing = event.clone();
    } else {
        next.push(event.clone());
    }

    next.sort_by(|a, b| b.ts.cmp(&a.ts));
    next
}

fn push_recent_activity(current: &[ActivityEvent], event: ActivityEvent) -> Vec<ActivityEvent> {
    if matches!(event.kind, ActivityKind::ActivitySample) {
        return current.to_vec();
    }

    let mut queue: VecDeque<ActivityEvent> = current.iter().cloned().collect();
    queue.push_front(event);

    while queue.len() > RECENT_ACTIVITY_LIMIT {
        queue.pop_back();
    }

    queue.into_iter().collect()
}

fn derive_status_from_activity(event: &ActivityEvent) -> DeviceStatus {
    let status_text = if event.presence == PresenceState::Locked {
        "屏幕已锁定".to_string()
    } else if event.presence == PresenceState::Idle {
        "当前空闲中".to_string()
    } else if let Some(browser) = &event.browser {
        if let Some(domain) = &browser.domain {
            format!("正在使用 {} 浏览 {}", event.app.name, domain)
        } else if let Some(page_title) = &browser.page_title {
            format!("正在使用 {} 查看 {}", event.app.name, page_title)
        } else {
            format!("正在使用 {}", event.app.name)
        }
    } else if let Some(window_title) = &event.window_title {
        format!("正在使用 {} · {}", event.app.name, window_title)
    } else {
        format!("正在使用 {}", event.app.name)
    };

    DeviceStatus {
        ts: event.ts,
        device_id: event.device_id.clone(),
        agent_name: event.agent_name.clone(),
        platform: event.platform.clone(),
        status_text,
        source: "auto-activity".to_string(),
    }
}

fn build_device_analysis_payload(
    device_id: String,
    latest_status: Option<DeviceStatus>,
    activities: Vec<ActivityEvent>,
    now: OffsetDateTime,
    range: AnalysisRange,
) -> DeviceAnalysisResponse {
    let mut app_usage = HashMap::new();
    let mut domain_usage = HashMap::new();
    let mut browser_usage = HashMap::new();
    let mut total_tracked_ms = 0_u64;
    let mut work_tracked_ms = 0_u64;
    let mut browser_tracked_ms = 0_u64;
    let mut event_count = 0_usize;
    let window_start = range.window_start(now);
    let local_offset = UtcOffset::current_local_offset()
        .ok()
        .unwrap_or(UtcOffset::UTC);
    let mut app_keys = HashSet::new();

    for (index, activity) in activities.iter().enumerate() {
        let next_ts = activities.get(index + 1).map(|next| next.ts).unwrap_or(now);
        let Some((effective_start, effective_end)) =
            effective_segment_bounds(activity.ts, next_ts, window_start, now)
        else {
            continue;
        };
        let tracked_ms = duration_between(effective_start, effective_end);
        if tracked_ms == 0 {
            continue;
        }

        if matches!(activity.kind, ActivityKind::ForegroundChanged)
            && activity.presence == PresenceState::Active
        {
            event_count += 1;
        }

        if activity.presence != PresenceState::Active {
            continue;
        }

        total_tracked_ms += tracked_ms;
        work_tracked_ms += duration_within_workday(effective_start, effective_end, local_offset);
        if activity.browser.is_some() {
            browser_tracked_ms += tracked_ms;
        }
        accumulate_app_usage(&mut app_usage, activity, tracked_ms);
        accumulate_domain_usage(&mut domain_usage, activity, tracked_ms);
        accumulate_browser_usage(&mut browser_usage, activity, tracked_ms);
        app_keys.insert(app_usage_key(activity));
    }

    let current_label = activities
        .iter()
        .rev()
        .find(|activity| activity.presence == PresenceState::Active)
        .map(current_activity_label)
        .or_else(|| activities.last().map(current_activity_label));

    DeviceAnalysisResponse {
        device_id,
        generated_at: now,
        total_tracked_ms,
        work_tracked_ms,
        browser_tracked_ms,
        app_count: app_keys.len(),
        event_count,
        current_label,
        latest_status,
        app_usage: finalize_usage_map(app_usage),
        domain_usage: finalize_usage_map(domain_usage),
        browser_usage: finalize_browser_usage_map(browser_usage),
    }
}

#[cfg(test)]
fn duration_within_window(
    start: OffsetDateTime,
    end: OffsetDateTime,
    window_start: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> u64 {
    let Some((effective_start, effective_end)) =
        effective_segment_bounds(start, end, window_start, now)
    else {
        return 0;
    };
    duration_between(effective_start, effective_end)
}

fn effective_segment_bounds(
    start: OffsetDateTime,
    end: OffsetDateTime,
    window_start: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let continuity_end = start + MAX_ACTIVITY_CONTINUITY;
    let effective_start = match window_start {
        Some(cutoff) if start < cutoff => cutoff,
        _ => start,
    };
    let effective_end = if end > now { now } else { end }.min(continuity_end);

    if effective_end <= effective_start {
        return None;
    }

    Some((effective_start, effective_end))
}

fn duration_between(start: OffsetDateTime, end: OffsetDateTime) -> u64 {
    (end - start)
        .whole_milliseconds()
        .max(0)
        .try_into()
        .unwrap_or(0)
}

fn duration_within_workday(
    start: OffsetDateTime,
    end: OffsetDateTime,
    local_offset: UtcOffset,
) -> u64 {
    let start_local = start.to_offset(local_offset);
    let end_local = end.to_offset(local_offset);

    let mut total = overlap_with_workday_for_date(start_local.date(), start_local, end_local);
    if end_local.date() != start_local.date() {
        total += overlap_with_workday_for_date(end_local.date(), start_local, end_local);
    }
    total
}

fn overlap_with_workday_for_date(
    date: time::Date,
    start_local: OffsetDateTime,
    end_local: OffsetDateTime,
) -> u64 {
    let work_start = date.midnight().assume_offset(start_local.offset())
        + TimeDuration::hours(WORKDAY_START_HOUR);
    let work_end =
        date.midnight().assume_offset(start_local.offset()) + TimeDuration::hours(WORKDAY_END_HOUR);
    let effective_start = start_local.max(work_start);
    let effective_end = end_local.min(work_end);

    if effective_end <= effective_start {
        return 0;
    }

    duration_between(effective_start, effective_end)
}

fn app_usage_key(activity: &ActivityEvent) -> String {
    if activity.browser.is_some() {
        format!("app:{}", activity.app.id)
    } else {
        let label = activity
            .window_title
            .clone()
            .or_else(|| activity.app.title.clone())
            .unwrap_or_else(|| activity.app.name.clone());
        format!("app:{}:{}", activity.app.id, label.to_lowercase())
    }
}

fn accumulate_app_usage(
    target: &mut HashMap<String, UsageBucket>,
    activity: &ActivityEvent,
    tracked_ms: u64,
) {
    let is_browser_activity = activity.browser.is_some();
    let label = if is_browser_activity {
        activity.app.name.clone()
    } else {
        activity
            .window_title
            .clone()
            .or_else(|| activity.app.title.clone())
            .unwrap_or_else(|| activity.app.name.clone())
    };

    let sublabel = if is_browser_activity || label == activity.app.name {
        Some(activity.app.id.clone())
    } else {
        Some(activity.app.name.clone())
    };

    let key = if is_browser_activity {
        format!("app:{}", activity.app.id)
    } else {
        format!("app:{}:{}", activity.app.id, label.to_lowercase())
    };

    merge_usage_entry(
        target,
        UsageBucket {
            key,
            label,
            sublabel,
            total_tracked_ms: tracked_ms,
            sessions: 1,
            last_seen: activity.ts,
        },
    );
}

fn accumulate_domain_usage(
    target: &mut HashMap<String, UsageBucket>,
    activity: &ActivityEvent,
    tracked_ms: u64,
) {
    let Some(browser) = &activity.browser else {
        return;
    };
    let Some(domain) = browser.domain.as_ref() else {
        return;
    };

    let sublabel = browser
        .page_title
        .clone()
        .or_else(|| Some(activity.app.name.clone()));

    merge_usage_entry(
        target,
        UsageBucket {
            key: domain.to_lowercase(),
            label: domain.clone(),
            sublabel,
            total_tracked_ms: tracked_ms,
            sessions: 1,
            last_seen: activity.ts,
        },
    );
}

#[derive(Debug, Clone)]
struct PageUsageAccumulator {
    key: String,
    label: String,
    url: Option<String>,
    total_tracked_ms: u64,
    sessions: u32,
    last_seen: OffsetDateTime,
}

#[derive(Debug, Clone)]
struct DomainUsageAccumulator {
    key: String,
    label: String,
    total_tracked_ms: u64,
    sessions: u32,
    last_seen: OffsetDateTime,
    pages: HashMap<String, PageUsageAccumulator>,
}

#[derive(Debug, Clone)]
struct BrowserUsageAccumulator {
    key: String,
    label: String,
    family: String,
    total_tracked_ms: u64,
    sessions: u32,
    last_seen: OffsetDateTime,
    domains: HashMap<String, DomainUsageAccumulator>,
}

fn accumulate_browser_usage(
    target: &mut HashMap<String, BrowserUsageAccumulator>,
    activity: &ActivityEvent,
    tracked_ms: u64,
) {
    let Some(browser) = activity.browser.as_ref() else {
        return;
    };

    let browser_key = format!(
        "browser:{}:{}",
        browser.family,
        activity.app.id.to_ascii_lowercase()
    );
    let browser_entry =
        target
            .entry(browser_key.clone())
            .or_insert_with(|| BrowserUsageAccumulator {
                key: browser_key,
                label: activity.app.name.clone(),
                family: browser.family.clone(),
                total_tracked_ms: 0,
                sessions: 0,
                last_seen: activity.ts,
                domains: HashMap::new(),
            });

    browser_entry.total_tracked_ms += tracked_ms;
    browser_entry.sessions += 1;
    if activity.ts >= browser_entry.last_seen {
        browser_entry.last_seen = activity.ts;
        browser_entry.label = activity.app.name.clone();
    }

    let Some(domain) = browser.domain.as_ref() else {
        return;
    };

    let domain_key = domain.to_ascii_lowercase();
    let domain_entry = browser_entry
        .domains
        .entry(domain_key.clone())
        .or_insert_with(|| DomainUsageAccumulator {
            key: domain_key,
            label: domain.clone(),
            total_tracked_ms: 0,
            sessions: 0,
            last_seen: activity.ts,
            pages: HashMap::new(),
        });

    domain_entry.total_tracked_ms += tracked_ms;
    domain_entry.sessions += 1;
    if activity.ts >= domain_entry.last_seen {
        domain_entry.last_seen = activity.ts;
        domain_entry.label = domain.clone();
    }

    let page_key = browser
        .url
        .clone()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| {
            browser
                .page_title
                .clone()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| domain.clone())
        });
    let page_label = browser
        .page_title
        .clone()
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| domain.clone());

    let page_entry = domain_entry
        .pages
        .entry(page_key.clone())
        .or_insert_with(|| PageUsageAccumulator {
            key: page_key,
            label: page_label.clone(),
            url: browser.url.clone(),
            total_tracked_ms: 0,
            sessions: 0,
            last_seen: activity.ts,
        });

    page_entry.total_tracked_ms += tracked_ms;
    page_entry.sessions += 1;
    if activity.ts >= page_entry.last_seen {
        page_entry.last_seen = activity.ts;
        page_entry.label = page_label;
        page_entry.url = browser.url.clone();
    }
}

fn merge_usage_vec(target: &mut HashMap<String, UsageBucket>, source: &[UsageBucket]) {
    for bucket in source {
        merge_usage_entry(target, bucket.clone());
    }
}

fn merge_browser_usage_vec(
    target: &mut HashMap<String, BrowserUsageAccumulator>,
    source: &[BrowserUsageBucket],
) {
    for browser in source {
        let browser_entry =
            target
                .entry(browser.key.clone())
                .or_insert_with(|| BrowserUsageAccumulator {
                    key: browser.key.clone(),
                    label: browser.label.clone(),
                    family: browser.family.clone(),
                    total_tracked_ms: 0,
                    sessions: 0,
                    last_seen: browser.last_seen,
                    domains: HashMap::new(),
                });

        browser_entry.total_tracked_ms += browser.total_tracked_ms;
        browser_entry.sessions += browser.sessions;
        if browser.last_seen >= browser_entry.last_seen {
            browser_entry.last_seen = browser.last_seen;
            browser_entry.label = browser.label.clone();
            browser_entry.family = browser.family.clone();
        }

        for domain in &browser.domains {
            let domain_entry = browser_entry
                .domains
                .entry(domain.key.clone())
                .or_insert_with(|| DomainUsageAccumulator {
                    key: domain.key.clone(),
                    label: domain.label.clone(),
                    total_tracked_ms: 0,
                    sessions: 0,
                    last_seen: domain.last_seen,
                    pages: HashMap::new(),
                });

            domain_entry.total_tracked_ms += domain.total_tracked_ms;
            domain_entry.sessions += domain.sessions;
            if domain.last_seen >= domain_entry.last_seen {
                domain_entry.last_seen = domain.last_seen;
                domain_entry.label = domain.label.clone();
            }

            for page in &domain.pages {
                let page_entry = domain_entry
                    .pages
                    .entry(page.key.clone())
                    .or_insert_with(|| PageUsageAccumulator {
                        key: page.key.clone(),
                        label: page.label.clone(),
                        url: page.url.clone(),
                        total_tracked_ms: 0,
                        sessions: 0,
                        last_seen: page.last_seen,
                    });

                page_entry.total_tracked_ms += page.total_tracked_ms;
                page_entry.sessions += page.sessions;
                if page.last_seen >= page_entry.last_seen {
                    page_entry.last_seen = page.last_seen;
                    page_entry.label = page.label.clone();
                    page_entry.url = page.url.clone();
                }
            }
        }
    }
}

fn merge_usage_entry(target: &mut HashMap<String, UsageBucket>, incoming: UsageBucket) {
    match target.get_mut(&incoming.key) {
        Some(existing) => {
            existing.total_tracked_ms += incoming.total_tracked_ms;
            existing.sessions += incoming.sessions;
            if incoming.last_seen > existing.last_seen {
                existing.last_seen = incoming.last_seen;
                existing.sublabel = incoming.sublabel;
            }
        }
        None => {
            target.insert(incoming.key.clone(), incoming);
        }
    }
}

fn finalize_usage_map(map: HashMap<String, UsageBucket>) -> Vec<UsageBucket> {
    let mut items = map.into_values().collect::<Vec<_>>();
    items.sort_by(|a, b| {
        b.total_tracked_ms
            .cmp(&a.total_tracked_ms)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    items.truncate(ANALYSIS_TOP_LIMIT);
    items
}

fn finalize_browser_usage_map(
    map: HashMap<String, BrowserUsageAccumulator>,
) -> Vec<BrowserUsageBucket> {
    let mut items = map
        .into_values()
        .map(|browser| BrowserUsageBucket {
            key: browser.key,
            label: browser.label,
            family: browser.family,
            total_tracked_ms: browser.total_tracked_ms,
            sessions: browser.sessions,
            last_seen: browser.last_seen,
            domains: finalize_domain_usage_map(browser.domains),
        })
        .collect::<Vec<_>>();

    items.sort_by(|a, b| {
        b.total_tracked_ms
            .cmp(&a.total_tracked_ms)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    items.truncate(ANALYSIS_TOP_LIMIT);
    items
}

fn finalize_domain_usage_map(
    map: HashMap<String, DomainUsageAccumulator>,
) -> Vec<DomainUsageBucket> {
    let mut items = map
        .into_values()
        .map(|domain| DomainUsageBucket {
            key: domain.key,
            label: domain.label,
            total_tracked_ms: domain.total_tracked_ms,
            sessions: domain.sessions,
            last_seen: domain.last_seen,
            pages: finalize_page_usage_map(domain.pages),
        })
        .collect::<Vec<_>>();

    items.sort_by(|a, b| {
        b.total_tracked_ms
            .cmp(&a.total_tracked_ms)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    items.truncate(ANALYSIS_TOP_LIMIT);
    items
}

fn finalize_page_usage_map(map: HashMap<String, PageUsageAccumulator>) -> Vec<PageUsageBucket> {
    let mut items = map
        .into_values()
        .map(|page| PageUsageBucket {
            key: page.key,
            label: page.label,
            url: page.url,
            total_tracked_ms: page.total_tracked_ms,
            sessions: page.sessions,
            last_seen: page.last_seen,
        })
        .collect::<Vec<_>>();

    items.sort_by(|a, b| {
        b.total_tracked_ms
            .cmp(&a.total_tracked_ms)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    items.truncate(ANALYSIS_TOP_LIMIT);
    items
}

fn current_activity_label(activity: &ActivityEvent) -> String {
    match activity.presence {
        PresenceState::Locked => "屏幕已锁定".to_string(),
        PresenceState::Idle => "当前空闲中".to_string(),
        PresenceState::Active => activity
            .browser
            .as_ref()
            .and_then(|browser| browser.page_title.clone())
            .or_else(|| activity.window_title.clone())
            .or_else(|| activity.app.title.clone())
            .unwrap_or_else(|| activity.app.name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use eyes_on_me_shared::{
        ActivityApp, ActivityEvent, ActivityKind, BrowserContext, Platform, PresenceState,
    };

    use super::{
        AnalysisRange, MAX_ACTIVITY_CONTINUITY, accumulate_app_usage, accumulate_browser_usage,
        build_device_analysis_payload, duration_within_window, finalize_browser_usage_map,
    };
    use time::{Date, Duration as TimeDuration, Month, OffsetDateTime, UtcOffset};

    #[test]
    fn parses_expected_analysis_ranges() {
        assert_eq!(AnalysisRange::from_query(None), Some(AnalysisRange::Today));
        assert_eq!(
            AnalysisRange::from_query(Some("3h")),
            Some(AnalysisRange::Last3Hours)
        );
        assert_eq!(
            AnalysisRange::from_query(Some("6h")),
            Some(AnalysisRange::Last6Hours)
        );
        assert_eq!(
            AnalysisRange::from_query(Some("today")),
            Some(AnalysisRange::Today)
        );
        assert_eq!(
            AnalysisRange::from_query(Some("1d")),
            Some(AnalysisRange::LastDay)
        );
        assert_eq!(
            AnalysisRange::from_query(Some("1w")),
            Some(AnalysisRange::LastWeek)
        );
        assert_eq!(
            AnalysisRange::from_query(Some("1m")),
            Some(AnalysisRange::LastMonth)
        );
        assert_eq!(
            AnalysisRange::from_query(Some("all")),
            Some(AnalysisRange::All)
        );
        assert_eq!(AnalysisRange::from_query(Some("bogus")), None);
    }

    #[test]
    fn today_range_uses_local_day_boundary() {
        let offset = UtcOffset::from_hms(8, 0, 0).expect("valid offset");
        let local_date = Date::from_calendar_date(2026, Month::March, 27).expect("valid date");
        let local_now =
            (local_date.midnight() + TimeDuration::hours(11) + TimeDuration::minutes(7))
                .assume_offset(offset);
        let now = local_now.to_offset(UtcOffset::UTC);

        let window_start = AnalysisRange::Today
            .window_start_with_offset(now, Some(offset))
            .expect("today should have a window start");

        assert_eq!(window_start, local_date.midnight().assume_offset(offset));
    }

    #[test]
    fn truncates_activity_duration_to_window_start() {
        let now = OffsetDateTime::now_utc();
        let start = now - TimeDuration::hours(4);
        let end = now - TimeDuration::hours(1);
        let cutoff = Some(now - TimeDuration::hours(3));

        let tracked_ms = duration_within_window(start, end, cutoff, now);

        assert_eq!(tracked_ms, 0);
    }

    #[test]
    fn ignores_segments_outside_window() {
        let now = OffsetDateTime::now_utc();
        let start = now - TimeDuration::hours(6);
        let end = now - TimeDuration::hours(5);
        let cutoff = Some(now - TimeDuration::hours(3));

        assert_eq!(duration_within_window(start, end, cutoff, now), 0);
    }

    #[test]
    fn groups_browser_activity_under_browser_app_in_app_usage() {
        let now = OffsetDateTime::now_utc();
        let activity = ActivityEvent {
            event_id: "evt-1".to_string(),
            ts: now,
            device_id: "windows-agent".to_string(),
            agent_name: "client-desktop".to_string(),
            platform: Platform::Windows,
            kind: ActivityKind::ForegroundChanged,
            app: ActivityApp {
                id: "msedge.exe".to_string(),
                name: "Microsoft Edge".to_string(),
                title: None,
                pid: Some(1234),
            },
            window_title: Some("Chaoleme/Eyes-on-me and 45 more pages - Personal".to_string()),
            browser: Some(BrowserContext {
                family: "chromium".to_string(),
                name: "Microsoft Edge".to_string(),
                page_title: Some("Chaoleme/Eyes-on-me and 45 more pages - Personal".to_string()),
                url: Some("https://github.com/Chaoleme/Eyes-on-me".to_string()),
                domain: Some("github.com".to_string()),
                source: "window-title".to_string(),
                confidence: 0.9,
            }),
            presence: PresenceState::Active,
            source: "desktop".to_string(),
        };

        let mut usage = HashMap::new();
        accumulate_app_usage(&mut usage, &activity, 60_000);

        let bucket = usage.get("app:msedge.exe").expect("browser app bucket");
        assert_eq!(bucket.label, "Microsoft Edge");
        assert_eq!(bucket.sublabel.as_deref(), Some("msedge.exe"));
    }

    #[test]
    fn caps_large_gaps_between_events() {
        let now = OffsetDateTime::now_utc();
        let start = now - TimeDuration::hours(8);
        let end = start + TimeDuration::hours(2);

        let tracked_ms = duration_within_window(start, end, None, now);

        assert_eq!(
            tracked_ms,
            MAX_ACTIVITY_CONTINUITY.whole_milliseconds() as u64
        );
    }

    #[test]
    fn builds_nested_browser_usage() {
        let now = OffsetDateTime::now_utc();
        let activity = ActivityEvent {
            event_id: "evt-2".to_string(),
            ts: now,
            device_id: "mac-agent".to_string(),
            agent_name: "client-desktop".to_string(),
            platform: Platform::Macos,
            kind: ActivityKind::ForegroundChanged,
            app: ActivityApp {
                id: "com.google.Chrome".to_string(),
                name: "Google Chrome".to_string(),
                title: None,
                pid: Some(777),
            },
            window_title: Some("Docs".to_string()),
            browser: Some(BrowserContext {
                family: "chromium".to_string(),
                name: "Google Chrome".to_string(),
                page_title: Some("Engineering Spec".to_string()),
                url: Some("https://docs.example.com/spec".to_string()),
                domain: Some("docs.example.com".to_string()),
                source: "test".to_string(),
                confidence: 0.95,
            }),
            presence: PresenceState::Active,
            source: "desktop".to_string(),
        };

        let mut usage = HashMap::new();
        accumulate_browser_usage(&mut usage, &activity, 120_000);
        let browsers = finalize_browser_usage_map(usage);

        assert_eq!(browsers.len(), 1);
        assert_eq!(browsers[0].label, "Google Chrome");
        assert_eq!(browsers[0].domains.len(), 1);
        assert_eq!(browsers[0].domains[0].label, "docs.example.com");
        assert_eq!(browsers[0].domains[0].pages[0].label, "Engineering Spec");
        assert_eq!(
            browsers[0].domains[0].pages[0].url.as_deref(),
            Some("https://docs.example.com/spec")
        );
    }

    #[test]
    fn keeps_latest_active_activity_for_current_label() {
        let now = OffsetDateTime::now_utc();
        let activities = vec![
            ActivityEvent {
                event_id: "evt-older".to_string(),
                ts: now - TimeDuration::minutes(5),
                device_id: "mac-agent".to_string(),
                agent_name: "client-desktop".to_string(),
                platform: Platform::Macos,
                kind: ActivityKind::ForegroundChanged,
                app: ActivityApp {
                    id: "com.google.Chrome".to_string(),
                    name: "Google Chrome".to_string(),
                    title: None,
                    pid: Some(777),
                },
                window_title: Some("Older page".to_string()),
                browser: None,
                presence: PresenceState::Active,
                source: "desktop".to_string(),
            },
            ActivityEvent {
                event_id: "evt-newer".to_string(),
                ts: now - TimeDuration::seconds(10),
                device_id: "mac-agent".to_string(),
                agent_name: "client-desktop".to_string(),
                platform: Platform::Macos,
                kind: ActivityKind::ForegroundChanged,
                app: ActivityApp {
                    id: "com.google.Chrome".to_string(),
                    name: "Google Chrome".to_string(),
                    title: None,
                    pid: Some(777),
                },
                window_title: Some("Latest page".to_string()),
                browser: None,
                presence: PresenceState::Active,
                source: "desktop".to_string(),
            },
            ActivityEvent {
                event_id: "evt-idle".to_string(),
                ts: now - TimeDuration::seconds(5),
                device_id: "mac-agent".to_string(),
                agent_name: "client-desktop".to_string(),
                platform: Platform::Macos,
                kind: ActivityKind::PresenceChanged,
                app: ActivityApp {
                    id: "com.google.Chrome".to_string(),
                    name: "Google Chrome".to_string(),
                    title: None,
                    pid: Some(777),
                },
                window_title: Some("Idle page".to_string()),
                browser: None,
                presence: PresenceState::Idle,
                source: "desktop".to_string(),
            },
        ];

        let analysis = build_device_analysis_payload(
            "mac-agent".to_string(),
            None,
            activities,
            now,
            AnalysisRange::All,
        );

        assert_eq!(analysis.current_label.as_deref(), Some("Latest page"));
    }
}
