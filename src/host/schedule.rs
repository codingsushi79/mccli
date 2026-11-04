//! Scheduled tasks: restarts, backups, broadcasts and commands on a timer.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, TimeZone, Timelike};

use super::Host;
use crate::proto::{Schedule, TaskAction, When, now};

/// The first time `when` fires strictly after `after`.
pub fn next_after(when: When, after: i64) -> i64 {
    match when {
        When::Every { minutes } => after + minutes.max(1) as i64 * 60,
        When::Daily { hour, minute } => {
            let base = Local
                .timestamp_opt(after, 0)
                .single()
                .unwrap_or_else(Local::now);
            let today = base
                .with_hour(hour as u32)
                .and_then(|t| t.with_minute(minute as u32))
                .and_then(|t| t.with_second(0))
                .map(|t| t.timestamp())
                .unwrap_or(after + 86400);
            if today > after { today } else { today + 86400 }
        }
    }
}

/// Run one task now. Returns what happened, for the task list.
pub fn run(host: &Arc<Host>, task: &Schedule) -> Result<String, String> {
    let instance = host
        .instance(&task.server)
        .ok_or_else(|| format!("server `{}` no longer exists", task.server))?;
    let result = match task.action {
        TaskAction::Command => instance.send_command(&task.payload).map(|_| format!("ran `{}`", task.payload)),
        TaskAction::Say => instance
            .send_command(&format!("say {}", task.payload))
            .map(|_| "broadcast sent".to_string()),
        TaskAction::Start => instance.start(),
        TaskAction::Stop => instance.stop(),
        TaskAction::Restart => {
            if instance.is_running() && !task.payload.trim().is_empty() {
                let _ = instance.send_command(&format!("say {}", task.payload));
            }
            instance.restart()
        }
        TaskAction::Backup => super::backup::spawn_create(
            instance.clone(),
            if task.payload.is_empty() {
                format!("scheduled: {}", task.name)
            } else {
                task.payload.clone()
            },
            format!("task:{}", task.name),
        ),
    };
    let outcome = result.map_err(|e| format!("{e:#}"));
    instance.log(format!(
        "task `{}` ({}): {}",
        task.name,
        task.action,
        match &outcome {
            Ok(m) => m.clone(),
            Err(e) => format!("failed — {e}"),
        }
    ));
    outcome
}

pub fn spawn_runner(host: Arc<Host>) {
    std::thread::Builder::new()
        .name("mchost-scheduler".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let due: Vec<Schedule> = {
                let tasks = host.schedules.lock().unwrap();
                let t = now();
                tasks
                    .tasks
                    .iter()
                    .filter(|s| s.enabled && s.next_run.is_some_and(|n| n <= t))
                    .cloned()
                    .collect()
            };
            for task in due {
                let outcome = run(&host, &task);
                let mut tasks = host.schedules.lock().unwrap();
                if let Some(entry) = tasks.tasks.iter_mut().find(|s| s.id == task.id) {
                    let t = now();
                    entry.last_run = Some(t);
                    entry.last_result = Some(match outcome {
                        Ok(m) => m,
                        Err(e) => format!("error: {e}"),
                    });
                    entry.next_run = Some(next_after(entry.when, t));
                }
                tasks.save();
            }
        })
        .expect("spawning the scheduler");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_add_up() {
        assert_eq!(next_after(When::Every { minutes: 30 }, 1000), 1000 + 1800);
    }

    #[test]
    fn daily_lands_on_the_next_occurrence() {
        let base = Local.with_ymd_and_hms(2026, 3, 10, 5, 0, 0).unwrap().timestamp();
        let next = next_after(When::Daily { hour: 4, minute: 0 }, base);
        let at = Local.timestamp_opt(next, 0).unwrap();
        assert_eq!((at.hour(), at.minute()), (4, 0));
        assert!(next > base && next - base <= 86400);

        let later_today = next_after(When::Daily { hour: 6, minute: 30 }, base);
        assert_eq!(later_today - base, 90 * 60);
    }
}
