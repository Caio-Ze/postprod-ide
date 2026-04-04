use crate::Dashboard;

use gpui::{
    App, Context, IntoElement, ParentElement, SharedString, Styled, Window, prelude::*,
};
use postprod_scheduler::RunResult;
use ui::{
    ButtonLike, ButtonSize, ButtonStyle, Color, ContextMenu, DropdownMenu, DropdownStyle, Icon,
    IconButton, IconName, IconSize, Label, LabelSize, Tooltip, prelude::*,
};
use util::ResultExt as _;


// ---------------------------------------------------------------------------
// Schedule helpers (pure functions, no Dashboard access)
// ---------------------------------------------------------------------------

pub(crate) const SCHEDULE_INTERVALS: &[&str] = &[
    "Every hour",
    "Every 2 hours",
    "Every 4 hours",
    "Every 6 hours",
    "Every 12 hours",
    "Every day",
    "Every week",
    "Every month",
];

pub(crate) fn cron_from_interval_and_hour(interval: &str, hour: u32, day: Option<u32>) -> String {
    let h = hour % 24;
    match interval {
        "Every hour" => "0 * * * *".to_string(),
        "Every 2 hours" => {
            let hours: Vec<String> = (0..12).map(|i| ((h + i * 2) % 24).to_string()).collect();
            format!("0 {} * * *", hours.join(","))
        }
        "Every 4 hours" => {
            let hours: Vec<String> = (0..6).map(|i| ((h + i * 4) % 24).to_string()).collect();
            format!("0 {} * * *", hours.join(","))
        }
        "Every 6 hours" => {
            let hours: Vec<String> = (0..4).map(|i| ((h + i * 6) % 24).to_string()).collect();
            format!("0 {} * * *", hours.join(","))
        }
        "Every 12 hours" => format!("0 {},{} * * *", h, (h + 12) % 24),
        "Every day" => format!("0 {} * * *", h),
        "Every week" => format!("0 {} * * {}", h, day.unwrap_or(1) % 7),
        "Every month" => format!("0 {} {} * *", h, day.unwrap_or(1).clamp(1, 31)),
        _ => format!("0 {} * * *", h), // fallback to daily
    }
}

pub(crate) fn interval_from_cron(cron: &str) -> &'static str {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() < 5 {
        return "Every day";
    }
    let hour_field = parts[1];
    let dom = parts[2];
    let dow = parts[4];

    if hour_field == "*" {
        return "Every hour";
    }
    if dom != "*" {
        return "Every month";
    }
    if dow != "*" {
        return "Every week";
    }

    let comma_count = hour_field.matches(',').count();
    match comma_count {
        0 => "Every day",
        1 => "Every 12 hours",
        3 => "Every 6 hours",
        5 => "Every 4 hours",
        11 => "Every 2 hours",
        _ => "Every day",
    }
}

pub(crate) fn hour_from_cron(cron: &str) -> u32 {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() < 2 {
        return 3;
    }
    let hour_str = parts[1].split(',').next().unwrap_or("3");
    hour_str.parse().unwrap_or(3)
}

pub(crate) fn format_hour_ampm(hour: u32) -> String {
    let h = hour % 24;
    match h {
        0 => "12:00 AM".to_string(),
        1..=11 => format!("{}:00 AM", h),
        12 => "12:00 PM".to_string(),
        _ => format!("{}:00 PM", h - 12),
    }
}

pub(crate) fn dow_from_cron(cron: &str) -> u32 {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() < 5 {
        return 1;
    }
    parts[4].parse().unwrap_or(1)
}

pub(crate) fn dom_from_cron(cron: &str) -> u32 {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() < 4 {
        return 1;
    }
    parts[2].parse().unwrap_or(1)
}

pub(crate) const DAY_OF_WEEK_LABELS: &[&str] = &[
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

pub(crate) fn format_dom(day: u32) -> String {
    match day {
        1 | 21 | 31 => format!("{day}st"),
        2 | 22 => format!("{day}nd"),
        3 | 23 => format!("{day}rd"),
        _ => format!("{day}th"),
    }
}

pub(crate) fn schedule_summary(cron: &str) -> String {
    let interval = interval_from_cron(cron);
    let hour = hour_from_cron(cron);
    if interval == "Every hour" {
        return "Every hour".to_string();
    }
    match interval {
        "Every week" => {
            let dow = dow_from_cron(cron) as usize;
            let day_name = DAY_OF_WEEK_LABELS.get(dow).unwrap_or(&"Monday");
            format!("Every {} at {}", day_name, format_hour_ampm(hour))
        }
        "Every month" => {
            let dom = dom_from_cron(cron);
            format!("Every month on the {} at {}", format_dom(dom), format_hour_ampm(hour))
        }
        _ => format!("{} at {}", interval, format_hour_ampm(hour)),
    }
}

// ---------------------------------------------------------------------------
// Scheduler UI rendering (impl Dashboard in a separate file)
// ---------------------------------------------------------------------------

impl Dashboard {
    pub(crate) fn render_scheduled_section(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let scheduled: Vec<_> = self.automations.iter()
            .filter(|a| a.schedule.as_ref().is_some_and(|s| s.enabled))
            .collect();

        if scheduled.is_empty() {
            return v_flex().w_full();
        }

        let is_open = !self.collapsed_sections.contains("scheduled");
        let status_map = self.scheduler.read(cx).status().clone();
        let entity = cx.entity().downgrade();

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if is_open {
            for entry in &scheduled {
                let cron = entry.schedule.as_ref()
                    .map(|s| s.cron.as_str())
                    .unwrap_or("");
                let summary: SharedString = schedule_summary(cron).into();
                let status = status_map.get(&entry.id);

                let (status_label, status_color) = match status.and_then(|s| s.last_result.as_ref()) {
                    Some(RunResult::Success) => ("OK", Color::Success),
                    Some(RunResult::Failed { .. }) => ("Failed", Color::Error),
                    Some(RunResult::Timeout) => ("Timeout", Color::Warning),
                    Some(RunResult::Skipped { .. }) => ("Skipped", Color::Muted),
                    None => ("Pending", Color::Muted),
                };

                let is_auto_disabled = status.is_some_and(|s| s.auto_disabled);

                let (status_label, status_color) = if is_auto_disabled {
                    let failures = status.map(|s| s.consecutive_failures).unwrap_or(0);
                    (format!("Auto-disabled ({failures} failures)"), Color::Error)
                } else {
                    (status_label.to_string(), status_color)
                };

                let pause_entity = entity.clone();
                let pause_id = entry.id.clone();

                let is_pipeline = entry.is_pipeline();

                let row = h_flex()
                    .id(SharedString::from(format!("sched-row-{}", entry.id)))
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .items_center()
                    .child(
                        Label::new(entry.label.clone())
                            .size(LabelSize::Small)
                            .color(if is_auto_disabled { Color::Disabled } else { Color::Default }),
                    )
                    .when(is_pipeline, |el| {
                        el.child(
                            Label::new(format!("{} steps", entry.steps.len()))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(summary)
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(status_label)
                            .color(status_color)
                            .size(LabelSize::XSmall),
                    )
                    .when(is_auto_disabled, {
                        let re_enable_entity = entity.clone();
                        let re_enable_id = entry.id.clone();
                        move |el| {
                            el.child(
                                ButtonLike::new(SharedString::from(format!("re-enable-{}", re_enable_id)))
                                    .style(ButtonStyle::Outlined)
                                    .child(
                                        Label::new("Re-enable")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Warning),
                                    )
                                    .on_click(move |_, _window, cx| {
                                        let re_enable_id = re_enable_id.clone();
                                        re_enable_entity.update(cx, |this, cx| {
                                            this.scheduler.update(cx, |scheduler, cx| {
                                                scheduler.re_enable(&re_enable_id, cx);
                                            });
                                            cx.notify();
                                        }).log_err();
                                    }),
                            )
                        }
                    })
                    .when(!is_auto_disabled, move |el| {
                        el.child(
                            IconButton::new(
                                format!("sched-pause-{}", pause_id),
                                IconName::CountdownTimer,
                            )
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Accent)
                            .tooltip(Tooltip::text("Disable schedule"))
                            .on_click(move |_, _window, cx| {
                                let pause_id = pause_id.clone();
                                pause_entity.update(cx, |this, cx| {
                                    this.toggle_schedule(&pause_id, cx);
                                }).log_err();
                            }),
                        )
                    });

                rows.push(row.into_any_element());
            }
        }

        v_flex()
            .w_full()
            .gap_1()
            .child(self.section_header("SCHEDULED", "scheduled", cx))
            .when(is_open, |el| {
                el.child(
                    v_flex()
                        .id("scheduled-content")
                        .w_full()
                        .gap_0p5()
                        .children(rows),
                )
            })
    }

    pub(crate) fn render_schedule_controls(
        &self,
        automation_id: &str,
        cron: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current_interval = interval_from_cron(cron);
        let current_hour = hour_from_cron(cron);
        let current_dow = dow_from_cron(cron);
        let current_dom = dom_from_cron(cron);

        let entity = cx.entity().downgrade();

        // Determine which day value to preserve when other controls change
        let current_day: Option<u32> = match current_interval {
            "Every week" => Some(current_dow),
            "Every month" => Some(current_dom),
            _ => None,
        };

        // Interval dropdown
        let interval_hour = current_hour;
        let interval_menu = ContextMenu::build(window, cx, {
            let auto_id = automation_id.to_string();
            let entity = entity.clone();
            move |mut menu, _window, _cx| {
                for &interval in SCHEDULE_INTERVALS {
                    let entity = entity.clone();
                    let auto_id = auto_id.clone();
                    let interval_str = interval.to_string();
                    // When switching intervals, use sensible day defaults
                    let day_for_interval: Option<u32> = match interval {
                        "Every week" => Some(current_day.unwrap_or(1)),
                        "Every month" => Some(current_day.unwrap_or(1)),
                        _ => None,
                    };
                    menu = menu.entry(
                        interval.to_string(),
                        None,
                        move |_window, cx: &mut App| {
                            entity.update(cx, |this: &mut Dashboard, cx| {
                                this.update_schedule_cron(&auto_id, &interval_str, interval_hour, day_for_interval, cx);
                            }).log_err();
                        },
                    );
                }
                menu
            }
        });

        // Time dropdown
        let time_menu = ContextMenu::build(window, cx, {
            let auto_id = automation_id.to_string();
            let interval = current_interval.to_string();
            let day = current_day;
            let entity = entity.clone();
            move |mut menu, _window, _cx| {
                for hour in 0..24u32 {
                    let entity = entity.clone();
                    let auto_id = auto_id.clone();
                    let interval = interval.clone();
                    let label = format_hour_ampm(hour);
                    menu = menu.entry(
                        label,
                        None,
                        move |_window, cx: &mut App| {
                            entity.update(cx, |this: &mut Dashboard, cx| {
                                this.update_schedule_cron(&auto_id, &interval, hour, day, cx);
                            }).log_err();
                        },
                    );
                }
                menu
            }
        });

        let show_time = current_interval != "Every hour";
        let show_dow = current_interval == "Every week";
        let show_dom = current_interval == "Every month";

        // Day-of-week dropdown (for weekly)
        let dow_dropdown = if show_dow {
            let dow_menu = ContextMenu::build(window, cx, {
                let auto_id = automation_id.to_string();
                let interval = current_interval.to_string();
                let entity = entity.clone();
                move |mut menu, _window, _cx| {
                    for (idx, &day_name) in DAY_OF_WEEK_LABELS.iter().enumerate() {
                        let entity = entity.clone();
                        let auto_id = auto_id.clone();
                        let interval = interval.clone();
                        let dow = idx as u32;
                        menu = menu.entry(
                            day_name.to_string(),
                            None,
                            move |_window, cx: &mut App| {
                                entity.update(cx, |this: &mut Dashboard, cx| {
                                    this.update_schedule_cron(&auto_id, &interval, interval_hour, Some(dow), cx);
                                }).log_err();
                            },
                        );
                    }
                    menu
                }
            });
            let dow_label = DAY_OF_WEEK_LABELS.get(current_dow as usize).unwrap_or(&"Monday");
            Some((dow_menu, dow_label.to_string()))
        } else {
            None
        };

        // Day-of-month dropdown (for monthly)
        let dom_dropdown = if show_dom {
            let dom_menu = ContextMenu::build(window, cx, {
                let auto_id = automation_id.to_string();
                let interval = current_interval.to_string();
                move |mut menu, _window, _cx| {
                    for day in 1..=31u32 {
                        let entity = entity.clone();
                        let auto_id = auto_id.clone();
                        let interval = interval.clone();
                        let label = format_dom(day);
                        menu = menu.entry(
                            label,
                            None,
                            move |_window, cx: &mut App| {
                                entity.update(cx, |this: &mut Dashboard, cx| {
                                    this.update_schedule_cron(&auto_id, &interval, interval_hour, Some(day), cx);
                                }).log_err();
                            },
                        );
                    }
                    menu
                }
            });
            Some((dom_menu, format_dom(current_dom)))
        } else {
            None
        };

        let auto_id_for_ids = automation_id.to_string();

        h_flex()
            .w_full()
            .pl(DynamicSpacing::Base48.rems(cx))
            .pr_2()
            .pb_2()
            .gap_2()
            .items_center()
            .child(
                Icon::new(IconName::CountdownTimer)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
            .child(
                DropdownMenu::new(
                    SharedString::from(format!("sched-interval-{}", auto_id_for_ids)),
                    current_interval.to_string(),
                    interval_menu,
                )
                .trigger_size(ButtonSize::None)
                .style(DropdownStyle::Outlined),
            )
            .when_some(dow_dropdown, |el, (menu, label)| {
                el.child(
                    Label::new("on")
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                )
                .child(
                    DropdownMenu::new(
                        SharedString::from(format!("sched-dow-{}", auto_id_for_ids)),
                        label,
                        menu,
                    )
                    .trigger_size(ButtonSize::None)
                    .style(DropdownStyle::Outlined),
                )
            })
            .when_some(dom_dropdown, |el, (menu, label)| {
                el.child(
                    Label::new("on the")
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                )
                .child(
                    DropdownMenu::new(
                        SharedString::from(format!("sched-dom-{}", auto_id_for_ids)),
                        label,
                        menu,
                    )
                    .trigger_size(ButtonSize::None)
                    .style(DropdownStyle::Outlined),
                )
            })
            .when(show_time, |el| {
                el.child(
                    Label::new("at")
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                )
                .child(
                    DropdownMenu::new(
                        SharedString::from(format!("sched-time-{}", auto_id_for_ids)),
                        format_hour_ampm(current_hour),
                        time_menu,
                    )
                    .trigger_size(ButtonSize::None)
                    .style(DropdownStyle::Outlined),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_from_interval_daily() {
        let cron = cron_from_interval_and_hour("Every day", 3, None);
        assert_eq!(cron, "0 3 * * *");
    }

    #[test]
    fn test_cron_from_interval_hourly() {
        let cron = cron_from_interval_and_hour("Every hour", 0, None);
        assert_eq!(cron, "0 * * * *");
    }

    #[test]
    fn test_cron_from_interval_weekly() {
        let cron = cron_from_interval_and_hour("Every week", 9, None);
        assert_eq!(cron, "0 9 * * 1");
    }

    #[test]
    fn test_cron_from_interval_weekly_with_day() {
        let cron = cron_from_interval_and_hour("Every week", 9, Some(3));
        assert_eq!(cron, "0 9 * * 3"); // Wednesday
    }

    #[test]
    fn test_cron_from_interval_monthly() {
        let cron = cron_from_interval_and_hour("Every month", 14, None);
        assert_eq!(cron, "0 14 1 * *");
    }

    #[test]
    fn test_cron_from_interval_monthly_with_day() {
        let cron = cron_from_interval_and_hour("Every month", 14, Some(15));
        assert_eq!(cron, "0 14 15 * *"); // 15th of month
    }

    #[test]
    fn test_cron_from_interval_12_hours() {
        let cron = cron_from_interval_and_hour("Every 12 hours", 3, None);
        assert_eq!(cron, "0 3,15 * * *");
    }

    #[test]
    fn test_interval_from_cron_daily() {
        assert_eq!(interval_from_cron("0 3 * * *"), "Every day");
    }

    #[test]
    fn test_interval_from_cron_hourly() {
        assert_eq!(interval_from_cron("0 * * * *"), "Every hour");
    }

    #[test]
    fn test_interval_from_cron_weekly() {
        assert_eq!(interval_from_cron("0 9 * * 1"), "Every week");
    }

    #[test]
    fn test_interval_from_cron_monthly() {
        assert_eq!(interval_from_cron("0 14 1 * *"), "Every month");
    }

    #[test]
    fn test_hour_from_cron() {
        assert_eq!(hour_from_cron("0 3 * * *"), 3);
        assert_eq!(hour_from_cron("0 14 1 * *"), 14);
        assert_eq!(hour_from_cron("0 * * * *"), 3); // "*" can't parse as number, defaults to 3
    }

    #[test]
    fn test_format_hour_ampm() {
        assert_eq!(format_hour_ampm(0), "12:00 AM");
        assert_eq!(format_hour_ampm(1), "1:00 AM");
        assert_eq!(format_hour_ampm(12), "12:00 PM");
        assert_eq!(format_hour_ampm(13), "1:00 PM");
        assert_eq!(format_hour_ampm(23), "11:00 PM");
    }

    #[test]
    fn test_schedule_summary() {
        assert_eq!(schedule_summary("0 3 * * *"), "Every day at 3:00 AM");
        assert_eq!(schedule_summary("0 * * * *"), "Every hour");
        assert_eq!(schedule_summary("0 14 1 * *"), "Every month on the 1st at 2:00 PM");
        assert_eq!(schedule_summary("0 9 * * 3"), "Every Wednesday at 9:00 AM");
        assert_eq!(schedule_summary("0 9 * * 0"), "Every Sunday at 9:00 AM");
        assert_eq!(schedule_summary("0 14 15 * *"), "Every month on the 15th at 2:00 PM");
    }

    #[test]
    fn test_dow_from_cron() {
        assert_eq!(dow_from_cron("0 9 * * 3"), 3); // Wednesday
        assert_eq!(dow_from_cron("0 9 * * 0"), 0); // Sunday
        assert_eq!(dow_from_cron("0 9 * * 1"), 1); // Monday (default)
    }

    #[test]
    fn test_dom_from_cron() {
        assert_eq!(dom_from_cron("0 14 15 * *"), 15);
        assert_eq!(dom_from_cron("0 14 1 * *"), 1);
    }

    #[test]
    fn test_format_dom() {
        assert_eq!(format_dom(1), "1st");
        assert_eq!(format_dom(2), "2nd");
        assert_eq!(format_dom(3), "3rd");
        assert_eq!(format_dom(4), "4th");
        assert_eq!(format_dom(11), "11th");
        assert_eq!(format_dom(21), "21st");
        assert_eq!(format_dom(22), "22nd");
        assert_eq!(format_dom(31), "31st");
    }

    #[test]
    fn test_cron_round_trip() {
        for &interval in SCHEDULE_INTERVALS {
            for hour in [0, 3, 12, 23] {
                let cron = cron_from_interval_and_hour(interval, hour, None);
                let recovered_interval = interval_from_cron(&cron);
                assert_eq!(
                    recovered_interval, interval,
                    "round-trip failed for {interval} at hour {hour}: cron='{cron}'"
                );
                if interval != "Every hour" {
                    let recovered_hour = hour_from_cron(&cron);
                    assert_eq!(
                        recovered_hour, hour,
                        "hour round-trip failed for {interval} at hour {hour}: cron='{cron}'"
                    );
                }
            }
        }
    }

    #[test]
    fn test_cron_round_trip_with_day() {
        // Weekly with specific days
        for dow in 0..7u32 {
            let cron = cron_from_interval_and_hour("Every week", 9, Some(dow));
            assert_eq!(interval_from_cron(&cron), "Every week");
            assert_eq!(hour_from_cron(&cron), 9);
            assert_eq!(dow_from_cron(&cron), dow);
        }
        // Monthly with specific days
        for dom in [1, 5, 15, 28, 31] {
            let cron = cron_from_interval_and_hour("Every month", 14, Some(dom));
            assert_eq!(interval_from_cron(&cron), "Every month");
            assert_eq!(hour_from_cron(&cron), 14);
            assert_eq!(dom_from_cron(&cron), dom);
        }
    }
}
