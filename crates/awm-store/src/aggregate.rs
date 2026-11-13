//! Turn raw watcher events into the numbers and ribbons the UI draws.
//!
//! Active time is window-watcher time with AFK intervals removed. When two
//! window events overlap on the same machine, the one that started later wins
//! that stretch — that is the window that actually had focus. Machines are
//! never merged into each other: two computers can be active at once, and the
//! totals add.

use std::collections::HashMap;

use awm_proto::{
    AppTotal, CategoryRule, CategoryTotal, DaySummary, Lane, MachineInfo, MachineTotal, PageTotal,
    Segment, SiteTotal, Summary, TitleTotal,
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;

use crate::StoredEvent;

const MAX_DURATION_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_DAYS: i64 = 400;
const TOP_APPS: usize = 30;
const TOP_TITLES: usize = 25;
const TOP_DAY_TITLES: usize = 12;
const TOP_SITES: usize = 12;
const TOP_PAGES: usize = 40;
const TOP_DAY_SITES: usize = 8;
const OTHER_COLOR: &str = "#9a8f84";

struct Slice {
    start: i64,
    end: i64,
    app: String,
    title: String,
    category: Option<String>,
}

struct CatInfo {
    name: String,
}

pub struct AggregateInput {
    pub events: Vec<StoredEvent>,
    pub machines: Vec<MachineInfo>,
    pub start_ms: i64,
    pub end_ms: i64,
    pub timezone: String,
    pub categories: Vec<CategoryRule>,
    pub databases: Vec<String>,
    pub last_ingest_ms: Option<i64>,
}

pub fn build(input: AggregateInput) -> Result<Summary, String> {
    if input.end_ms <= input.start_ms {
        return Err("start must be before end".into());
    }
    let tz: Tz = input.timezone.parse().unwrap_or(chrono_tz::UTC);
    let timezone = tz.to_string();

    let first = local_date(input.start_ms, tz).ok_or("start is out of range")?;
    let last = local_date(input.end_ms - 1, tz).ok_or("end is out of range")?;
    let day_count = (last - first).num_days() + 1;
    if day_count > MAX_DAYS {
        return Err(format!(
            "range is {day_count} days; refocus to {MAX_DAYS} or fewer"
        ));
    }

    let cat_index = category_index(&input.categories);
    let hostnames = hostname_map(&input.machines);

    let mut windows: HashMap<String, Vec<&StoredEvent>> = HashMap::new();
    let mut afk_events: HashMap<String, Vec<&StoredEvent>> = HashMap::new();
    let mut webs: Vec<&StoredEvent> = Vec::new();
    for event in &input.events {
        match event.bucket_type.as_str() {
            "currentwindow" => windows
                .entry(event.device_id.clone())
                .or_default()
                .push(event),
            "afkstatus" => afk_events
                .entry(event.device_id.clone())
                .or_default()
                .push(event),
            other if other.starts_with("web.tab") => webs.push(event),
            _ => {}
        }
    }

    let mut device_ids: Vec<String> = windows.keys().chain(afk_events.keys()).cloned().collect();
    device_ids.sort();
    device_ids.dedup();

    let afk_by_device: HashMap<String, Vec<(i64, i64)>> = device_ids
        .iter()
        .map(|id| {
            let intervals = afk_intervals(afk_events.get(id).map(Vec::as_slice).unwrap_or(&[]));
            (id.clone(), intervals)
        })
        .collect();

    let slices_by_device: HashMap<String, Vec<Slice>> = device_ids
        .iter()
        .map(|id| {
            let afk = afk_by_device.get(id).map(Vec::as_slice).unwrap_or(&[]);
            let slices = window_slices(
                windows.get(id).map(Vec::as_slice).unwrap_or(&[]),
                afk,
                &cat_index,
            );
            (id.clone(), slices)
        })
        .collect();

    let mut apps: HashMap<String, i64> = HashMap::new();
    let mut titles: HashMap<(String, String), i64> = HashMap::new();
    let mut cats: HashMap<String, i64> = HashMap::new();
    let mut sites: HashMap<String, i64> = HashMap::new();
    let mut pages: HashMap<String, PageAcc> = HashMap::new();
    let web_devices: std::collections::HashSet<&str> =
        webs.iter().map(|event| event.device_id.as_str()).collect();
    let mut machine_active: HashMap<String, i64> = HashMap::new();
    let mut active_ms = 0i64;
    let mut afk_ms = 0i64;
    let mut days = Vec::with_capacity(day_count as usize);

    let mut date = first;
    loop {
        let day_start = day_bounds_start(date, tz).ok_or("could not resolve local midnight")?;
        let next = date.succ_opt().ok_or("date overflow")?;
        let day_end = day_bounds_start(next, tz).ok_or("could not resolve next midnight")?;
        // The query range may start or end mid-day. Clip so partial days don't
        // pull in events the caller did not ask for.
        let clip_start = day_start.max(input.start_ms);
        let clip_end = day_end.min(input.end_ms);

        let mut day_active = 0i64;
        let mut day_afk = 0i64;
        let mut day_titles: HashMap<(String, String), i64> = HashMap::new();
        let mut day_sites: HashMap<String, i64> = HashMap::new();
        let mut lanes = Vec::new();
        let mut coverage: Vec<(i64, i64)> = Vec::new();

        for device_id in &device_ids {
            let afk = afk_by_device
                .get(device_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let afk_clipped = clip_intervals(afk, clip_start, clip_end);
            let device_afk: i64 = afk_clipped.iter().map(|(s, e)| e - s).sum();

            let slices = slices_by_device
                .get(device_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let clipped = clip_slices(slices, clip_start, clip_end);
            if clipped.is_empty() && afk_clipped.is_empty() {
                continue;
            }

            let spans = sweep(&clipped);
            let mut lane_active = 0i64;
            for span in &spans {
                let dur = span.end - span.start;
                lane_active += dur;
                day_active += dur;
                active_ms += dur;
                *machine_active.entry(device_id.clone()).or_default() += dur;
                *apps.entry(span.app.clone()).or_default() += dur;
                let cat_name = span.category.clone().unwrap_or_else(|| "Other".into());
                *cats.entry(cat_name).or_default() += dur;
                // Browser tab titles belong with websites, not the app window list.
                if !span.title.is_empty() && !is_browser(&span.app) {
                    *titles
                        .entry((span.app.clone(), span.title.clone()))
                        .or_default() += dur;
                    *day_titles
                        .entry((span.app.clone(), span.title.clone()))
                        .or_default() += dur;
                }
                coverage.push((span.start, span.end));
            }
            day_afk += device_afk;
            afk_ms += device_afk;

            let mut segments = merge_app_segments(&spans);
            segments.extend(afk_segments(&afk_clipped));
            segments.sort_by_key(|seg| seg.start_ms);

            if lane_active > 0 || device_afk > 0 {
                lanes.push(Lane {
                    device_id: device_id.clone(),
                    hostname: hostnames
                        .get(device_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| device_id.clone()),
                    segments,
                });
            }
        }

        for event in &webs {
            record_visit(
                event,
                &afk_by_device,
                clip_start,
                clip_end,
                &mut sites,
                &mut day_sites,
                &mut pages,
            );
        }
        for (device_id, events) in &windows {
            if web_devices.contains(device_id.as_str()) {
                continue;
            }
            for event in events {
                if !event.app.as_deref().is_some_and(is_browser) {
                    continue;
                }
                record_visit(
                    event,
                    &afk_by_device,
                    clip_start,
                    clip_end,
                    &mut sites,
                    &mut day_sites,
                    &mut pages,
                );
            }
        }

        let mini = if lanes.len() <= 1 {
            lanes
                .first()
                .map(|lane| lane.segments.clone())
                .unwrap_or_default()
        } else {
            merge_intervals(coverage)
                .into_iter()
                .map(|(start, end)| Segment {
                    start_ms: start,
                    end_ms: end,
                    kind: "app".into(),
                    app: Some("Active".into()),
                    title: None,
                    category: None,
                })
                .collect()
        };

        days.push(DaySummary {
            date: date.format("%Y-%m-%d").to_string(),
            start_ms: day_start,
            end_ms: day_end,
            active_secs: ms_to_secs(day_active),
            afk_secs: ms_to_secs(day_afk),
            segments: mini,
            lanes,
            titles: top_titles(&day_titles, TOP_DAY_TITLES),
            sites: top_sites(&day_sites, TOP_DAY_SITES),
        });

        if date >= last {
            break;
        }
        date = next;
    }

    let mut machines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for info in &input.machines {
        seen.insert(info.device_id.clone());
        machines.push(MachineTotal {
            device_id: info.device_id.clone(),
            hostname: info.hostname.clone(),
            last_seen_ms: info.last_seen_ms,
            active_secs: ms_to_secs(machine_active.get(&info.device_id).copied().unwrap_or(0)),
        });
    }
    for (device_id, ms) in &machine_active {
        if seen.contains(device_id) {
            continue;
        }
        machines.push(MachineTotal {
            device_id: device_id.clone(),
            hostname: hostnames
                .get(device_id.as_str())
                .cloned()
                .unwrap_or_else(|| device_id.clone()),
            last_seen_ms: 0,
            active_secs: ms_to_secs(*ms),
        });
    }
    machines.sort_by(|a, b| {
        b.active_secs
            .partial_cmp(&a.active_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.hostname.cmp(&b.hostname))
    });

    let app_category = |app: &str| -> Option<String> {
        cat_index.get(&app.to_lowercase()).map(|c| c.name.clone())
    };

    let mut app_totals: Vec<AppTotal> = apps
        .into_iter()
        .map(|(app, ms)| AppTotal {
            category: app_category(&app),
            app,
            secs: ms_to_secs(ms),
        })
        .collect();
    app_totals.sort_by(|a, b| {
        b.secs
            .partial_cmp(&a.secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    app_totals.truncate(TOP_APPS);

    let mut category_totals: Vec<CategoryTotal> = cats
        .into_iter()
        .map(|(name, ms)| CategoryTotal {
            color: category_color(&name, &input.categories),
            name,
            secs: ms_to_secs(ms),
        })
        .collect();
    category_totals.sort_by(|a, b| {
        b.secs
            .partial_cmp(&a.secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(Summary {
        start: DateTime::<Utc>::from_timestamp_millis(input.start_ms)
            .unwrap_or_else(Utc::now)
            .to_rfc3339(),
        end: DateTime::<Utc>::from_timestamp_millis(input.end_ms)
            .unwrap_or_else(Utc::now)
            .to_rfc3339(),
        timezone,
        active_secs: ms_to_secs(active_ms),
        afk_secs: ms_to_secs(afk_ms),
        event_count: input.events.len() as i64,
        databases: input.databases,
        last_ingest_ms: input.last_ingest_ms,
        machines,
        apps: app_totals,
        categories: category_totals,
        titles: top_titles(&titles, TOP_TITLES),
        sites: top_sites(&sites, TOP_SITES),
        pages: top_pages(&pages, TOP_PAGES),
        days,
    })
}

struct PageAcc {
    title: String,
    url: String,
    domain: String,
    ms: i64,
}

fn record_visit(
    event: &StoredEvent,
    afk_by_device: &HashMap<String, Vec<(i64, i64)>>,
    clip_start: i64,
    clip_end: i64,
    sites: &mut HashMap<String, i64>,
    day_sites: &mut HashMap<String, i64>,
    pages: &mut HashMap<String, PageAcc>,
) {
    let Some((start, end)) = event_span(event) else {
        return;
    };
    let url = event.url.clone().unwrap_or_default();
    let title = event.title.clone().unwrap_or_default();
    if url.is_empty() && title.is_empty() {
        return;
    }
    let domain = domain_of(&url).unwrap_or_else(|| {
        if title.is_empty() {
            "page".into()
        } else {
            title.clone()
        }
    });
    let afk = afk_by_device
        .get(&event.device_id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for (s, e) in subtract(start, end, afk) {
        let cs = s.max(clip_start);
        let ce = e.min(clip_end);
        if ce <= cs {
            continue;
        }
        let dur = ce - cs;
        *sites.entry(domain.clone()).or_default() += dur;
        *day_sites.entry(domain.clone()).or_default() += dur;
        let key = format!(
            "{domain}\n{}",
            if title.is_empty() {
                domain.as_str()
            } else {
                title.as_str()
            }
        );
        let page = pages.entry(key).or_insert_with(|| PageAcc {
            title: if title.is_empty() {
                domain.clone()
            } else {
                title.clone()
            },
            url: url.clone(),
            domain: domain.clone(),
            ms: 0,
        });
        page.ms += dur;
        if page.url.is_empty() && !url.is_empty() {
            page.url = url.clone();
        }
    }
}

fn category_index(rules: &[CategoryRule]) -> HashMap<String, CatInfo> {
    let mut map = HashMap::new();
    for rule in rules {
        for app in &rule.apps {
            map.entry(app.to_lowercase()).or_insert_with(|| CatInfo {
                name: rule.name.clone(),
            });
        }
    }
    map
}

fn category_color(name: &str, rules: &[CategoryRule]) -> String {
    if name == "Other" {
        return OTHER_COLOR.into();
    }
    rules
        .iter()
        .find(|rule| rule.name == name)
        .map(|rule| rule.color.clone())
        .unwrap_or_else(|| OTHER_COLOR.into())
}

fn hostname_map(machines: &[MachineInfo]) -> HashMap<&str, String> {
    machines
        .iter()
        .map(|m| (m.device_id.as_str(), m.hostname.clone()))
        .collect()
}

fn local_date(ms: i64, tz: Tz) -> Option<NaiveDate> {
    Some(
        DateTime::<Utc>::from_timestamp_millis(ms)?
            .with_timezone(&tz)
            .date_naive(),
    )
}

fn day_bounds_start(date: NaiveDate, tz: Tz) -> Option<i64> {
    let naive = date.and_hms_opt(0, 0, 0)?;
    let local = tz
        .from_local_datetime(&naive)
        .single()
        .or_else(|| tz.from_local_datetime(&naive).earliest())?;
    Some(local.timestamp_millis())
}

fn event_span(event: &StoredEvent) -> Option<(i64, i64)> {
    if !event.duration_secs.is_finite() || event.duration_secs <= 0.0 {
        return None;
    }
    let dur = (event.duration_secs * 1000.0).round() as i64;
    let dur = dur.clamp(1, MAX_DURATION_MS);
    Some((event.timestamp_ms, event.timestamp_ms.saturating_add(dur)))
}

fn is_afk(event: &StoredEvent) -> bool {
    event
        .afk_status
        .as_deref()
        .is_some_and(|status| status.eq_ignore_ascii_case("afk"))
}

fn afk_intervals(events: &[&StoredEvent]) -> Vec<(i64, i64)> {
    let mut intervals = Vec::new();
    for event in events {
        if !is_afk(event) {
            continue;
        }
        if let Some(span) = event_span(event) {
            intervals.push(span);
        }
    }
    merge_intervals(intervals)
}

fn window_slices(
    events: &[&StoredEvent],
    afk: &[(i64, i64)],
    cats: &HashMap<String, CatInfo>,
) -> Vec<Slice> {
    let mut slices = Vec::new();
    for event in events {
        let Some((start, end)) = event_span(event) else {
            continue;
        };
        let app = event
            .app
            .clone()
            .filter(|app| !app.is_empty())
            .unwrap_or_else(|| "(unknown)".into());
        let title = event.title.clone().unwrap_or_default();
        let category = cats.get(&app.to_lowercase()).map(|cat| cat.name.clone());
        for (s, e) in subtract(start, end, afk) {
            slices.push(Slice {
                start: s,
                end: e,
                app: app.clone(),
                title: title.clone(),
                category: category.clone(),
            });
        }
    }
    slices
}

fn clip_slices(slices: &[Slice], start: i64, end: i64) -> Vec<Slice> {
    let mut out = Vec::new();
    for slice in slices {
        let s = slice.start.max(start);
        let e = slice.end.min(end);
        if e > s {
            out.push(Slice {
                start: s,
                end: e,
                app: slice.app.clone(),
                title: slice.title.clone(),
                category: slice.category.clone(),
            });
        }
    }
    out
}

fn clip_intervals(intervals: &[(i64, i64)], start: i64, end: i64) -> Vec<(i64, i64)> {
    intervals
        .iter()
        .filter_map(|(s, e)| {
            let cs = (*s).max(start);
            let ce = (*e).min(end);
            (ce > cs).then_some((cs, ce))
        })
        .collect()
}

fn subtract(start: i64, end: i64, blocks: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut cursor = start;
    let mut out = Vec::new();
    for &(bs, be) in blocks {
        if be <= cursor {
            continue;
        }
        if bs >= end {
            break;
        }
        if bs > cursor {
            out.push((cursor, bs.min(end)));
        }
        cursor = cursor.max(be);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        out.push((cursor, end));
    }
    out
}

fn merge_intervals(mut intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    if intervals.is_empty() {
        return intervals;
    }
    intervals.sort_by_key(|interval| interval.0);
    let mut out = vec![intervals[0]];
    for (start, end) in intervals.into_iter().skip(1) {
        let last = out.last_mut().expect("out is non-empty");
        if start <= last.1 {
            last.1 = last.1.max(end);
        } else {
            out.push((start, end));
        }
    }
    out
}

/// Attribute overlapping window slices. The slice that started most recently
/// keeps the time; ties break on app name so the result is stable.
fn sweep(slices: &[Slice]) -> Vec<Slice> {
    if slices.is_empty() {
        return Vec::new();
    }
    let mut points: Vec<i64> = slices.iter().flat_map(|s| [s.start, s.end]).collect();
    points.sort_unstable();
    points.dedup();

    let mut spans = Vec::new();
    for pair in points.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if end <= start {
            continue;
        }
        let mut best: Option<&Slice> = None;
        for slice in slices {
            if slice.start <= start && slice.end > start {
                let replace = match best {
                    None => true,
                    Some(current) => {
                        slice.start > current.start
                            || (slice.start == current.start && slice.app > current.app)
                    }
                };
                if replace {
                    best = Some(slice);
                }
            }
        }
        if let Some(slice) = best {
            spans.push(Slice {
                start,
                end,
                app: slice.app.clone(),
                title: slice.title.clone(),
                category: slice.category.clone(),
            });
        }
    }
    spans
}

fn merge_app_segments(spans: &[Slice]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut title_weight: Vec<HashMap<String, i64>> = Vec::new();
    for span in spans {
        let dur = span.end - span.start;
        if let Some(last) = out.last_mut() {
            if last.kind == "app"
                && last.end_ms == span.start
                && last.app.as_deref() == Some(span.app.as_str())
            {
                last.end_ms = span.end;
                if let Some(weights) = title_weight.last_mut() {
                    *weights.entry(span.title.clone()).or_default() += dur;
                    last.title = dominant_title(weights);
                }
                continue;
            }
        }
        let mut weights = HashMap::new();
        *weights.entry(span.title.clone()).or_default() += dur;
        out.push(Segment {
            start_ms: span.start,
            end_ms: span.end,
            kind: "app".into(),
            app: Some(span.app.clone()),
            title: dominant_title(&weights),
            category: span.category.clone(),
        });
        title_weight.push(weights);
    }
    out
}

fn dominant_title(weights: &HashMap<String, i64>) -> Option<String> {
    weights
        .iter()
        .filter(|(title, _)| !title.is_empty())
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)))
        .map(|(title, _)| title.clone())
}

fn afk_segments(intervals: &[(i64, i64)]) -> Vec<Segment> {
    intervals
        .iter()
        .map(|(start, end)| Segment {
            start_ms: *start,
            end_ms: *end,
            kind: "afk".into(),
            app: None,
            title: Some("Idle".into()),
            category: None,
        })
        .collect()
}

fn top_titles(map: &HashMap<(String, String), i64>, limit: usize) -> Vec<TitleTotal> {
    let mut rows: Vec<TitleTotal> = map
        .iter()
        .filter(|((_, title), _)| !title.is_empty())
        .map(|((app, title), ms)| TitleTotal {
            app: app.clone(),
            title: title.clone(),
            secs: ms_to_secs(*ms),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.secs
            .partial_cmp(&a.secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit);
    rows
}

fn top_sites(map: &HashMap<String, i64>, limit: usize) -> Vec<SiteTotal> {
    let mut rows: Vec<SiteTotal> = map
        .iter()
        .map(|(domain, ms)| SiteTotal {
            domain: domain.clone(),
            secs: ms_to_secs(*ms),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.secs
            .partial_cmp(&a.secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit);
    rows
}

fn top_pages(map: &HashMap<String, PageAcc>, limit: usize) -> Vec<PageTotal> {
    let mut rows: Vec<PageTotal> = map
        .values()
        .map(|page| PageTotal {
            title: page.title.clone(),
            url: page.url.clone(),
            domain: page.domain.clone(),
            secs: ms_to_secs(page.ms),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.secs
            .partial_cmp(&a.secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit);
    rows
}

fn is_browser(app: &str) -> bool {
    matches!(
        app.to_ascii_lowercase().as_str(),
        "google chrome"
            | "chrome"
            | "chromium"
            | "safari"
            | "firefox"
            | "arc"
            | "brave browser"
            | "brave"
            | "microsoft edge"
            | "edge"
            | "orion"
            | "dia"
            | "vivaldi"
            | "helium"
            | "opera"
            | "opera gx"
    )
}

fn domain_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let scheme = scheme.to_ascii_lowercase();
    if matches!(
        scheme.as_str(),
        "chrome" | "brave" | "edge" | "about" | "chrome-extension" | "moz-extension"
    ) {
        let page = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let page = if page.is_empty() { "page" } else { page };
        return Some(format!("{scheme}://{page}"));
    }
    if !scheme.is_empty() && scheme != "http" && scheme != "https" {
        return Some(scheme);
    }
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.split('@').next_back()?;
    let host = host.split(':').next()?;
    let host = host.strip_prefix("www.").unwrap_or(host).trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn ms_to_secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(device: &str, start: i64, secs: f64, app: &str, title: &str) -> StoredEvent {
        StoredEvent {
            device_id: device.into(),
            bucket_id: format!("aw-watcher-window_{device}"),
            bucket_type: "currentwindow".into(),
            timestamp_ms: start,
            duration_secs: secs,
            app: Some(app.into()),
            title: Some(title.into()),
            url: None,
            afk_status: None,
        }
    }

    fn afk(device: &str, start: i64, secs: f64) -> StoredEvent {
        StoredEvent {
            device_id: device.into(),
            bucket_id: format!("aw-watcher-afk_{device}"),
            bucket_type: "afkstatus".into(),
            timestamp_ms: start,
            duration_secs: secs,
            app: None,
            title: None,
            url: None,
            afk_status: Some("afk".into()),
        }
    }

    #[test]
    fn later_window_wins_an_overlap() {
        let summary = build(AggregateInput {
            events: vec![
                window("dev", 0, 100.0, "Code", "a.rs"),
                window("dev", 50_000, 100.0, "Safari", "docs"),
            ],
            machines: vec![],
            start_ms: 0,
            end_ms: 200_000,
            timezone: "UTC".into(),
            categories: vec![],
            databases: vec![],
            last_ingest_ms: None,
        })
        .unwrap();

        assert!((summary.active_secs - 150.0).abs() < 0.01);
        let code = summary.apps.iter().find(|a| a.app == "Code").unwrap();
        let safari = summary.apps.iter().find(|a| a.app == "Safari").unwrap();
        assert!((code.secs - 50.0).abs() < 0.01);
        assert!((safari.secs - 100.0).abs() < 0.01);
    }

    #[test]
    fn afk_is_removed_from_active_time() {
        let summary = build(AggregateInput {
            events: vec![
                window("dev", 0, 3600.0, "Code", "main.rs"),
                afk("dev", 900_000, 900.0),
            ],
            machines: vec![],
            start_ms: 0,
            end_ms: 4_000_000,
            timezone: "UTC".into(),
            categories: vec![],
            databases: vec![],
            last_ingest_ms: None,
        })
        .unwrap();
        assert!((summary.active_secs - 2700.0).abs() < 0.01);
        assert!((summary.afk_secs - 900.0).abs() < 0.01);
    }

    #[test]
    fn browser_tabs_are_pages_not_window_titles() {
        let mut web = window("dev", 60_000, 60.0, "Google Chrome", "Inbox - Gmail");
        web.bucket_type = "web.tab.current".into();
        web.bucket_id = "aw-watcher-web-chrome".into();
        web.app = None;
        web.url = Some("https://mail.google.com/mail/u/0/#inbox".into());
        let summary = build(AggregateInput {
            events: vec![
                window("dev", 60_000, 60.0, "Google Chrome", "Inbox - Gmail"),
                window("dev", 0, 60.0, "Code", "main.rs"),
                web,
            ],
            machines: vec![],
            start_ms: 0,
            end_ms: 200_000,
            timezone: "UTC".into(),
            categories: vec![],
            databases: vec![],
            last_ingest_ms: None,
        })
        .unwrap();

        assert!(summary
            .titles
            .iter()
            .all(|title| title.app != "Google Chrome"));
        assert!(summary.titles.iter().any(|title| title.app == "Code"));
        assert!(summary
            .pages
            .iter()
            .any(|page| page.domain == "mail.google.com" && page.title.contains("Gmail")));
        assert!(summary
            .sites
            .iter()
            .any(|site| site.domain == "mail.google.com"));
    }
}
