//! Pinned todo bands and viewport margins.

use super::*;

/// Use the exact pinned-card identity and context for both drawing and queries.
fn project_pinned_todo_card<R>(
    payload: Option<&str>,
    diff_mode: crate::config::DiffDisplayMode,
    width: u16,
    project: impl FnOnce(&[Line<'static>]) -> R,
) -> R {
    if width < 16 || !crate::config::config().display.pin_todos {
        return project(&[]);
    }
    let Some(payload) = payload else {
        return project(&[]);
    };
    // Distinct title avoids sharing the width-capped transcript card's entry.
    let mut msg = crate::tui::DisplayMessage::todos(payload.to_string());
    msg.title = Some("Pinned todos".to_string());
    super::super::messages::project_cached_message_lines(
        &msg,
        width,
        diff_mode,
        super::super::messages::render_pinned_todos_message,
        project,
    )
}

/// Exact card visibility, without cloning cached lines, spinner animation, or
/// assembling a collapsed/expanded band. Malformed payloads still use the
/// renderer's system-message fallback rather than a separate JSON heuristic.
pub(super) fn pinned_todo_card_has_lines(
    payload: Option<&str>,
    diff_mode: crate::config::DiffDisplayMode,
    width: u16,
) -> bool {
    project_pinned_todo_card(payload, diff_mode, width, |lines| !lines.is_empty())
}

/// Pinned todo band for an arbitrary todo payload. Shared by the live chat
/// viewport and the `/resume` preview so both render the band identically.
#[cfg(test)]
pub(crate) fn pinned_todo_band_lines_for_payload(
    payload: Option<&str>,
    expanded: bool,
    diff_mode: crate::config::DiffDisplayMode,
    width: u16,
    viewport_height: u16,
) -> (Vec<Line<'static>>, Option<usize>) {
    pinned_todo_band_with_details(
        payload,
        expanded,
        &[],
        false,
        diff_mode,
        width,
        viewport_height,
    )
}

#[derive(Clone, Debug)]
pub(crate) enum PinnedTodoAction {
    Lane(String),
    Intent,
}

thread_local! {
    pub(super) static PINNED_TODO_ROW_ACTIONS: std::cell::RefCell<Vec<(usize, PinnedTodoAction)>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(super) static PINNED_TODO_CLICK_AREAS: std::cell::RefCell<Vec<(Rect, PinnedTodoAction)>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn pinned_todo_action_at(x: u16, y: u16) -> Option<PinnedTodoAction> {
    PINNED_TODO_CLICK_AREAS.with_borrow(|targets| {
        targets
            .iter()
            .find(|(area, _)| area.contains((x, y).into()))
            .map(|(_, action)| action.clone())
    })
}

#[cfg(test)]
pub(crate) fn pinned_todo_lane_area_for_test(key: &str) -> Option<Rect> {
    PINNED_TODO_CLICK_AREAS.with_borrow(|targets| {
        targets.iter().find_map(|(area, action)| {
            matches!(action, PinnedTodoAction::Lane(id) if id == key).then_some(*area)
        })
    })
}

pub(super) fn pinned_todo_band_with_details(
    payload: Option<&str>,
    expanded: bool,
    expanded_lanes: &[String],
    intent_expanded: bool,
    diff_mode: crate::config::DiffDisplayMode,
    width: u16,
    viewport_height: u16,
) -> (Vec<Line<'static>>, Option<usize>) {
    if !super::super::secondary_render_active() {
        PINNED_TODO_ROW_ACTIONS.with_borrow_mut(Vec::clear);
    }
    if width < 16 || viewport_height < 3 {
        return (Vec::new(), None);
    }

    let content_width = width.saturating_sub(3);
    let (card_lines, lane_targets) = if crate::config::config().display.pin_todos {
        payload.map_or_else(
            || (Vec::new(), Vec::new()),
            |payload| {
                // Pinned disclosure rendering has its own bounded cache, keyed by
                // payload, width, and the expanded lane ids.
                let mut msg = crate::tui::DisplayMessage::todos(payload.to_string());
                msg.title = Some("Pinned todos".to_string());
                let mut expanded_lanes = expanded_lanes.to_vec();
                if expanded {
                    expanded_lanes.push("*".to_string());
                }
                super::super::messages::render_pinned_todos_with_details(
                    &msg,
                    content_width,
                    diff_mode,
                    &expanded_lanes,
                )
            },
        )
    } else {
        (Vec::new(), Vec::new())
    };
    PINNED_TODO_LAST_MAX.store(0, std::sync::atomic::Ordering::Relaxed);
    if card_lines.is_empty() {
        return (Vec::new(), None);
    }
    let mut card_lines = card_lines;
    super::super::messages::animate_lane_spinners(&mut card_lines);

    let (mut intent, intent_can_unwrap) = super::super::messages::pinned_todo_intent(
        payload.unwrap_or_default(),
        content_width,
        intent_expanded,
    );
    let source_intent_rows = usize::from(!intent.is_empty());
    let padding = usize::from(viewport_height >= 9);
    let intent_gap = usize::from(!intent.is_empty() && viewport_height >= 12);
    let budget = (usize::from(viewport_height) * 2 / 3)
        .max(padding * 2 + intent_gap + source_intent_rows + 1)
        .min(usize::from(viewport_height).saturating_sub(1));
    // Long intent must not consume the task window or the conversation.
    let max_intent_rows = budget.saturating_sub(padding * 2 + intent_gap + 2).max(1);
    intent.truncate(max_intent_rows);
    if intent_can_unwrap && let Some(first) = intent.first_mut() {
        first.spans.insert(
            1.min(first.spans.len()),
            Span::styled(
                if intent_expanded { "▾ " } else { "▸ " },
                Style::default().fg(tool_color()),
            ),
        );
    }
    // Intent is fixed chrome, not part of the task list's scrolling window.
    let task_lines = &card_lines[source_intent_rows.min(card_lines.len())..];
    let chrome_rows = padding * 2 + intent.len() + intent_gap;
    // Leave transcript space even in short terminals. Extra padding yields to
    // content at small heights, and long plans scroll inside their own panel.
    let window = budget.saturating_sub(chrome_rows);
    let max_offset = if window > 0 {
        task_lines.len().saturating_sub(window)
    } else {
        0
    };
    let offset = pinned_todo_scroll().min(max_offset);
    PINNED_TODO_LAST_MAX.store(max_offset, std::sync::atomic::Ordering::Relaxed);

    let mut lines = vec![Line::raw(""); padding];
    let mut actions = Vec::new();
    if intent_can_unwrap {
        actions
            .extend((padding..padding + intent.len()).map(|row| (row, PinnedTodoAction::Intent)));
    }
    lines.extend(intent);
    lines.extend(std::iter::repeat_n(Line::raw(""), intent_gap));
    let task_start = lines.len();
    for (source_row, key) in lane_targets {
        let source_row = source_row.saturating_sub(source_intent_rows);
        if source_row >= offset && source_row < offset + window {
            actions.push((
                task_start + source_row - offset,
                PinnedTodoAction::Lane(key),
            ));
        }
    }
    lines.extend(task_lines.iter().skip(offset).take(window).cloned());
    if !super::super::secondary_render_active() {
        PINNED_TODO_ROW_ACTIONS.with_borrow_mut(|rows| *rows = actions);
    }
    lines.extend(std::iter::repeat_n(Line::raw(""), padding));
    let lines = lines
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::raw(" "));
            let mut line = super::super::truncate_line_with_ellipsis_to_width(
                &line,
                usize::from(width.saturating_sub(2)),
            );
            line.spans.push(Span::raw(
                " ".repeat(usize::from(width).saturating_sub(line.width())),
            ));
            line.style = line.style.bg(user_bg());
            // Task markers can carry Style::reset(), which otherwise punches
            // terminal-default holes through the panel's inherited surface.
            for span in &mut line.spans {
                span.style = span.style.bg(user_bg());
            }
            line
        })
        .collect();
    (lines, Some(task_start))
}

pub(crate) fn compute_visible_margins(
    lines: &[Line],
    visible_user_indices: &[usize],
    area: Rect,
    centered: bool,
) -> info_widget::Margins {
    let visible_height = area.height as usize;
    let mut visible_user_cursor = 0usize;

    let mut right_widths = Vec::with_capacity(visible_height);
    let mut left_widths = Vec::with_capacity(visible_height);

    for row in 0..visible_height {
        while visible_user_cursor < visible_user_indices.len()
            && visible_user_indices[visible_user_cursor] < row
        {
            visible_user_cursor += 1;
        }
        let is_user_line = visible_user_cursor < visible_user_indices.len()
            && visible_user_indices[visible_user_cursor] == row;

        if row < lines.len() {
            let mut used = lines[row].width().min(area.width as usize) as u16;
            if is_user_line && area.width > 0 {
                used = used.saturating_add(1).min(area.width);
            }

            // Compute the true free space on each side from the line's *rendered*
            // alignment. This matters even in left-aligned mode: the header lines
            // (`server:`/`client:`/model/version, auth status, mcp list, changelog
            // box, etc.) are always centered regardless of mode, so a centered line
            // of width `used` leaves only ~half the slack on the right. Reporting
            // the full `area.width - used` here would let a right-side info widget
            // overlap the centered header text.
            let total_margin = area.width.saturating_sub(used);
            let default_alignment = if centered {
                Alignment::Center
            } else {
                Alignment::Left
            };
            let effective_alignment = lines[row].alignment.unwrap_or(default_alignment);
            let (left_margin, right_margin) = match effective_alignment {
                Alignment::Left => (0, total_margin),
                Alignment::Center => {
                    let left = total_margin / 2;
                    let right = total_margin.saturating_sub(left);
                    (left, right)
                }
                Alignment::Right => (total_margin, 0),
            };

            if centered {
                left_widths.push(left_margin);
                right_widths.push(right_margin);
            } else {
                // Left-aligned mode never places left-side widgets (content is
                // flush-left), so the left gap is reported as 0; the right gap
                // still respects per-line alignment so widgets clear the header.
                left_widths.push(0);
                right_widths.push(right_margin);
            }
        } else if centered {
            let half = area.width / 2;
            left_widths.push(half);
            right_widths.push(area.width.saturating_sub(half));
        } else {
            left_widths.push(0);
            right_widths.push(area.width);
        }
    }

    info_widget::Margins {
        right_widths,
        left_widths,
        centered,
        ..Default::default()
    }
}

#[cfg(test)]
mod pinned_todo_projection_tests {
    use super::*;

    fn run_isolated(test: &str, pin_todos: bool) -> bool {
        if std::env::var_os("JCODE_PINNED_PROJECTION_CHILD").is_some() {
            assert_eq!(std::env::var("JCODE_TEST_SESSION").as_deref(), Ok("1"));
            assert_eq!(crate::config::config().display.pin_todos, pin_todos);
            return false;
        }
        let home = std::env::temp_dir().join(format!(
            "jcode-pinned-projection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!("[display]\npin_todos = {pin_todos}\n"),
        )
        .unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::{test}"),
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("JCODE_HOME", &home)
            .env("JCODE_TEST_SESSION", "1")
            .env("JCODE_PINNED_PROJECTION_CHILD", "1")
            .output()
            .unwrap();
        std::fs::remove_dir_all(home).unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{stdout}");
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success() && stdout.contains("1 passed;"),
            "isolated {test} failed or did not run"
        );
        true
    }

    fn payload(task_count: usize) -> String {
        serde_json::json!({
            "todos": (0..task_count).map(|i| serde_json::json!({
                "id": format!("task-{i}"),
                "content": format!("Implement and validate scoped task {i}"),
                "status": "in_progress", "priority": "high", "lane": "worker",
                "assigned_to": "session_fox_1700000000"
            })).collect::<Vec<_>>(),
            "plan": {}, "goals": [],
            "agents": [{"session_id": "session_fox_1700000000", "name": "fox", "status": "running", "activity": "Validate scoped task"}]
        }).to_string()
    }

    #[test]
    fn cached_visibility_matches_band_for_payload_width_diff_and_expansion() {
        if run_isolated(
            "cached_visibility_matches_band_for_payload_width_diff_and_expansion",
            true,
        ) {
            return;
        }
        let active = payload(32);
        let empty = payload(0);
        for payload in [
            None,
            Some(""),
            Some("not json"),
            Some("{"),
            Some("null"),
            Some("[]"),
            Some("{}"),
            Some(empty.as_str()),
            Some(active.as_str()),
        ] {
            for width in [0, 1, 15, 16, 17, 40, 120] {
                for diff in [
                    crate::config::DiffDisplayMode::Off,
                    crate::config::DiffDisplayMode::Inline,
                ] {
                    let visible = pinned_todo_card_has_lines(payload, diff, width);
                    for expanded in [false, true] {
                        let (lines, _) =
                            pinned_todo_band_lines_for_payload(payload, expanded, diff, width, 3);
                        assert_eq!(
                            visible,
                            !lines.is_empty(),
                            "payload={payload:?}, width={width}, expanded={expanded}"
                        );
                    }
                    if width < 16 || payload.is_none() || !crate::config::config().display.pin_todos
                    {
                        assert!(!visible);
                    }
                }
            }
        }
    }

    #[test]
    fn cached_visibility_respects_disabled_pinning() {
        if run_isolated("cached_visibility_respects_disabled_pinning", false) {
            return;
        }
        let active = payload(32);
        for width in [0, 15, 16, 120] {
            for payload in [None, Some("not json"), Some(active.as_str())] {
                assert!(!pinned_todo_card_has_lines(
                    payload,
                    crate::config::DiffDisplayMode::Off,
                    width
                ));
                assert!(
                    pinned_todo_band_lines_for_payload(
                        payload,
                        true,
                        crate::config::DiffDisplayMode::Off,
                        width,
                        3
                    )
                    .0
                    .is_empty()
                );
            }
        }
    }

    /// Same binary/profile, cache, payload and width. Alternate order per pair.
    /// This measures the hot visibility query only, not terminal frame time or
    /// allocations. Run serially with an isolated JCODE_HOME and selfdev profile.
    #[test]
    #[ignore = "paired pinned todo cache microbenchmark"]
    fn paired_cached_visibility_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;
        if run_isolated("paired_cached_visibility_benchmark", true) {
            return;
        }
        assert!(crate::config::config().display.pin_todos);
        let iterations = 10_000;
        for count in [1, 32, 128] {
            let payload = payload(count);
            for width in [40, 120] {
                for expanded in [false, true] {
                    let old = || {
                        !pinned_todo_band_lines_for_payload(
                            Some(black_box(payload.as_str())),
                            expanded,
                            crate::config::DiffDisplayMode::Off,
                            width,
                            3,
                        )
                        .0
                        .is_empty()
                    };
                    let new = || {
                        pinned_todo_card_has_lines(
                            Some(black_box(payload.as_str())),
                            crate::config::DiffDisplayMode::Off,
                            width,
                        )
                    };
                    assert_eq!(old(), new());
                    for _ in 0..100 {
                        black_box(old());
                        black_box(new());
                    }
                    let mut old_ns = Vec::new();
                    let mut new_ns = Vec::new();
                    let measure = |f: &dyn Fn() -> bool| {
                        let start = Instant::now();
                        for _ in 0..iterations {
                            black_box(f());
                        }
                        start.elapsed().as_nanos() / iterations
                    };
                    for pair in 0..7 {
                        if pair % 2 == 0 {
                            old_ns.push(measure(&old));
                            new_ns.push(measure(&new));
                        } else {
                            new_ns.push(measure(&new));
                            old_ns.push(measure(&old));
                        }
                    }
                    old_ns.sort_unstable();
                    new_ns.sort_unstable();
                    eprintln!(
                        "pinned_visibility_bench count={count} width={width} expanded={expanded} iterations={iterations} pairs=7 old_median_ns={} new_median_ns={} old_range={:?} new_range={:?}",
                        old_ns[3],
                        new_ns[3],
                        (old_ns[0], old_ns[6]),
                        (new_ns[0], new_ns[6])
                    );
                }
            }
        }
    }
}
