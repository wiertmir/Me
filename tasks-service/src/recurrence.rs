use std::collections::VecDeque;

use chrono::{DateTime, Datelike, Utc};
use common::{
    recur,
    time::{When, YEARS},
};
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::tasks::{
    COLUMNS, MAX_TASKS_PER_LIST, Task, bump, due_parts, from_row, live_count, passed, subtasks,
};

/// After a long absence only this many missed occurrences are created per chain, the most recent ones.
const MAX_MISSED: usize = 30;

/// Creates every task that is due for `user`'s chains at `now`. Never fails the request for a chain it
/// cannot read: that chain is skipped with a warning.
pub fn catch_up(c: &Connection, user: Uuid, now: DateTime<Utc>) -> rusqlite::Result<()> {
    let heads: Vec<Task> = c
        .prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE rrule IS NOT NULL AND deleted = 0 AND due_utc <= ?1
             AND list_id IN (SELECT id FROM lists WHERE user_id = ?2)"
        ))?
        .query_map(params![now.timestamp(), user.to_string()], from_row)?
        .collect::<rusqlite::Result<_>>()?;
    for head in &heads {
        advance(c, user, head, now)?;
    }
    Ok(())
}

/// Creates the tasks that follow `head`, whose due has passed: the missed ones and the upcoming one, which
/// becomes the head.
fn advance(c: &Connection, user: Uuid, head: &Task, now: DateTime<Utc>) -> rusqlite::Result<()> {
    let rule = head.rrule.as_deref().unwrap_or_default();
    let read = due_parts(head.due.as_deref().unwrap_or_default(), head.tz.as_deref())
        .and_then(|(start, tz, _)| Ok((start, tz, recur::build(rule, start, tz)?)));
    let (start, tz, set) = match read {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(event = "recurrence_skipped", task_id = %head.id, reason = %e.message);
            return Ok(());
        }
    };
    // A chain of COUNT=n is n tasks: each task carries one less than the one before it, skipped ones included.
    let count = set.get_rrule()[0].get_count();
    let first = recur::wall(start);
    let later = set
        // Gives up on a rule that produces nothing (30 February) after 100,000 steps, not at the year 9999.
        .limit()
        .into_iter()
        .skip_while(|dt| *dt <= first)
        .take(count.map_or(usize::MAX, |n| (n as usize).saturating_sub(1)))
        .take_while(|dt| YEARS.contains(&dt.year()))
        .map(|dt| match start {
            When::Date(_) => When::Date(dt.date_naive()),
            When::Timed(_) => When::Timed(dt.naive_utc()),
        })
        .zip(1u32..);
    // ponytail: walks every occurrence from the head's due to now, on every request, for a head that is not
    // advanced (its rule ended by UNTIL, or its list full); clear the rule of an ended chain to stop that
    let mut keep = VecDeque::new();
    for (w, nth) in later {
        let over = passed(w, tz);
        keep.push_back((w, over, nth));
        if over > now {
            break;
        }
        if keep.len() > MAX_MISSED {
            keep.pop_front();
        }
    }
    let ts = now.timestamp();
    let mut prev = head.id;
    for (w, over, nth) in keep {
        let kids = subtasks(c, prev)?;
        if live_count(c, head.list_id)? + 1 + kids.len() as i64 > MAX_TASKS_PER_LIST {
            break;
        }
        let rule = match count {
            Some(n) => with_count(rule, n - nth),
            None => rule.to_string(),
        };
        let due = Some((w.to_string(), over.timestamp()));
        let new = copy(c, head.list_id, head.id, None, due, Some(&rule), ts)?;
        c.execute(
            "UPDATE tasks SET rrule = NULL, revision = ?2, updated_at = ?3 WHERE id = ?1",
            params![prev.to_string(), bump(c, head.list_id)?, ts],
        )?;
        for kid in kids {
            copy(c, head.list_id, kid.id, Some(new), None, None, ts)?;
        }
        tracing::info!(event = "task_recurred", user_id = %user, list_id = %head.list_id, task_id = %new,
            follows = %prev);
        prev = new;
    }
    Ok(())
}

/// `rule` with its `COUNT` set to `n`; the rest of the string is kept as it is.
fn with_count(rule: &str, n: u32) -> String {
    rule.split(';')
        .map(|part| {
            let is_count = part
                .get(..6)
                .is_some_and(|k| k.eq_ignore_ascii_case("COUNT="));
            if is_count {
                format!("COUNT={n}")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Inserts an open copy of the stored task `from` under a new id and uid, as a write of its own, and returns
/// the id. The copy keeps the texts, zone, priority, reminders and `recurrence_id`; it keeps the due unless
/// `due` (with its `due_utc`) is given.
fn copy(
    c: &Connection,
    list: Uuid,
    from: Uuid,
    parent: Option<Uuid>,
    due: Option<(String, i64)>,
    rrule: Option<&str>,
    now: i64,
) -> rusqlite::Result<Uuid> {
    let id = Uuid::new_v4();
    let (due, due_utc) = due.unzip();
    c.execute(
        "INSERT INTO tasks (id, list_id, uid, summary, description, due, tz, priority, reminders, parent_id,
            rrule, recurrence_id, revision, due_utc, created_at, updated_at)
         SELECT ?2, list_id, ?2, summary, description, COALESCE(?3, due), tz, priority, reminders, ?4,
            ?5, recurrence_id, ?6, COALESCE(?7, due_utc), ?8, ?8 FROM tasks WHERE id = ?1",
        params![
            from.to_string(),
            id.to_string(),
            due,
            parent.map(|p| p.to_string()),
            rrule,
            bump(c, list)?,
            due_utc,
            now,
        ],
    )?;
    Ok(id)
}
