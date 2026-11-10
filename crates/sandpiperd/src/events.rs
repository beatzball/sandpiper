//! The event log (DESIGN.md §6). Every machine's events, with a sequence
//! number per machine, kept in SQLite so a client that disconnects can come
//! back with `from=<seq>` and miss nothing within the retention window.
//!
//! Writers append through one connection behind a lock, which also hands
//! out sequence numbers. Each appended event is broadcast to live readers;
//! a reader that falls behind reads the gap back from the database.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc};

/// How long events are kept.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// Events read from the database per query while replaying.
const PAGE: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub machine: String,
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub time: u64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

impl Event {
    /// One NDJSON line.
    pub fn line(&self) -> Bytes {
        let mut v = serde_json::to_vec(self).expect("events serialize");
        v.push(b'\n');
        Bytes::from(v)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn db_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(format!("event log: {e}"))
}

struct Inner {
    db: Connection,
    /// The next sequence number of each machine written to since start.
    next: HashMap<String, u64>,
}

pub struct Log {
    inner: Mutex<Inner>,
    live: broadcast::Sender<Arc<Event>>,
}

impl Log {
    pub fn open(path: &Path) -> io::Result<Self> {
        let db = Connection::open(path).map_err(db_err)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS events (
               machine TEXT NOT NULL,
               seq     INTEGER NOT NULL,
               time    INTEGER NOT NULL,
               type    TEXT NOT NULL,
               data    TEXT NOT NULL,
               PRIMARY KEY (machine, seq)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS events_time ON events (time);",
        )
        .map_err(db_err)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                db,
                next: HashMap::new(),
            }),
            live: broadcast::channel(1024).0,
        })
    }

    /// Append one event and return it with its sequence number.
    pub fn append(&self, machine: &str, kind: &str, data: Value) -> io::Result<Arc<Event>> {
        let data = match data {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => panic!("event data must be an object, not {other}"),
        };
        let mut inner = self.inner.lock().expect("event log lock");
        let seq = match inner.next.get(machine) {
            Some(&n) => n,
            None => {
                let last: Option<i64> = inner
                    .db
                    .query_row(
                        "SELECT MAX(seq) FROM events WHERE machine = ?1",
                        [machine],
                        |r| r.get(0),
                    )
                    .map_err(db_err)?;
                last.map_or(1, |n| n as u64 + 1)
            }
        };
        let event = Arc::new(Event {
            machine: machine.to_owned(),
            seq,
            time: now_ms(),
            kind: kind.to_owned(),
            data,
        });
        inner
            .db
            .execute(
                "INSERT INTO events (machine, seq, time, type, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    event.machine,
                    event.seq as i64,
                    event.time as i64,
                    event.kind,
                    Value::Object(event.data.clone()).to_string()
                ],
            )
            .map_err(db_err)?;
        if kind == "machine.destroyed" {
            // Nothing follows it, so the counter can go.
            inner.next.remove(machine);
        } else {
            inner.next.insert(machine.to_owned(), seq + 1);
        }
        drop(inner);
        let _ = self.live.send(Arc::clone(&event)); // no live readers is fine
        Ok(event)
    }

    /// Up to `limit` events of `machine` from `from` on, in order.
    pub fn read(&self, machine: &str, from: u64, limit: usize) -> io::Result<Vec<Event>> {
        let inner = self.inner.lock().expect("event log lock");
        let mut stmt = inner
            .db
            .prepare_cached(
                "SELECT seq, time, type, data FROM events
                 WHERE machine = ?1 AND seq >= ?2 ORDER BY seq LIMIT ?3",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![machine, from as i64, limit as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, time, kind, data) = row.map_err(db_err)?;
            let data = match serde_json::from_str(&data) {
                Ok(Value::Object(m)) => m,
                _ => return Err(io::Error::other("event log: corrupt event data")),
            };
            out.push(Event {
                machine: machine.to_owned(),
                seq,
                time,
                kind,
                data,
            });
        }
        Ok(out)
    }

    /// The newest sequence number of `machine`, or 0.
    pub fn max_seq(&self, machine: &str) -> io::Result<u64> {
        let inner = self.inner.lock().expect("event log lock");
        let last: Option<i64> = inner
            .db
            .query_row(
                "SELECT MAX(seq) FROM events WHERE machine = ?1",
                [machine],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        Ok(last.unwrap_or(0) as u64)
    }

    /// The newest event of `machine` whose type starts with `prefix`.
    pub fn last_of(&self, machine: &str, prefix: &str) -> io::Result<Option<Event>> {
        let inner = self.inner.lock().expect("event log lock");
        let seq: Option<i64> = inner
            .db
            .query_row(
                "SELECT seq FROM events WHERE machine = ?1 AND substr(type, 1, length(?2)) = ?2
                 ORDER BY seq DESC LIMIT 1",
                params![machine, prefix],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        drop(inner);
        match seq {
            Some(seq) => Ok(self.read(machine, seq as u64, 1)?.pop()),
            None => Ok(None),
        }
    }

    /// Machines with events but no `machine.destroyed`: left over from a
    /// host agent that did not stop cleanly.
    pub fn unfinished(&self) -> io::Result<Vec<String>> {
        let inner = self.inner.lock().expect("event log lock");
        let mut stmt = inner
            .db
            .prepare(
                "SELECT machine FROM events GROUP BY machine
                 HAVING SUM(type = 'machine.destroyed') = 0",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([], |r| r.get(0)).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    /// Delete events older than [`RETENTION`].
    pub fn purge(&self) -> io::Result<usize> {
        let cutoff = now_ms().saturating_sub(RETENTION.as_millis() as u64);
        let inner = self.inner.lock().expect("event log lock");
        inner
            .db
            .execute("DELETE FROM events WHERE time < ?1", [cutoff as i64])
            .map_err(db_err)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.live.subscribe()
    }
}

/// Stream `machine`'s events from `from` on as NDJSON: first from the
/// database, then live. Only events that pass `keep` are sent; the stream
/// ends after the first event that passes `last`, or when the client goes.
pub fn follow(
    log: Arc<Log>,
    machine: String,
    from: u64,
    keep: impl Fn(&Event) -> bool + Send + Sync + 'static,
    last: impl Fn(&Event) -> bool + Send + Sync + 'static,
) -> mpsc::Receiver<Result<Bytes, Infallible>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        // Subscribe before reading the database, so nothing falls between.
        let mut live = log.subscribe();
        // A cursor past the end starts at the end: nothing was missed.
        let end = {
            let (log, m) = (Arc::clone(&log), machine.clone());
            tokio::task::spawn_blocking(move || log.max_seq(&m)).await
        };
        let Ok(Ok(end)) = end else { return };
        let mut next = from.max(1).min(end + 1);
        'replay: loop {
            let page = {
                let (log, machine) = (Arc::clone(&log), machine.clone());
                tokio::task::spawn_blocking(move || log.read(&machine, next, PAGE)).await
            };
            let page = match page {
                Ok(Ok(page)) => page,
                Ok(Err(e)) => {
                    let _ = tx.send(Ok(stream_error(&e.to_string()))).await;
                    return;
                }
                Err(_) => return,
            };
            let full = page.len() == PAGE;
            for e in &page {
                next = e.seq + 1;
                if emit(&tx, e, &keep, &last).await {
                    return;
                }
            }
            if full {
                continue;
            }
            // Asked to start past the machine's end: nothing more will come.
            if page.is_empty() {
                let (log, m) = (Arc::clone(&log), machine.clone());
                let ended =
                    tokio::task::spawn_blocking(move || log.last_of(&m, "machine.destroyed")).await;
                if let Ok(Ok(Some(e))) = ended
                    && e.seq < next
                {
                    return;
                }
            }
            loop {
                let e = tokio::select! {
                    e = live.recv() => e,
                    () = tx.closed() => return,
                };
                match e {
                    Ok(e) if e.machine != machine || e.seq < next => {}
                    Ok(e) if e.seq == next => {
                        next += 1;
                        if emit(&tx, &e, &keep, &last).await {
                            return;
                        }
                    }
                    // A gap, or this reader fell behind: read it back.
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue 'replay,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    });
    rx
}

/// A last line that tells the client the stream broke, not ended.
fn stream_error(message: &str) -> Bytes {
    let mut v =
        serde_json::to_vec(&serde_json::json!({"type": "stream.error", "message": message}))
            .expect("serializes");
    v.push(b'\n');
    Bytes::from(v)
}

/// Send one event if `keep` passes it. Returns true when the stream is
/// done: the client went, or the event passed `last`.
async fn emit(
    tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    e: &Event,
    keep: &(impl Fn(&Event) -> bool + Sync),
    last: &(impl Fn(&Event) -> bool + Sync),
) -> bool {
    if keep(e) && tx.send(Ok(e.line())).await.is_err() {
        return true;
    }
    last(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_log() -> (Log, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("sandpiperd-test-{}", crate::ids::random_hex(8)));
        std::fs::create_dir_all(&dir).unwrap();
        (Log::open(&dir.join("events.db")).unwrap(), dir)
    }

    #[test]
    fn sequence_numbers_are_per_machine_and_survive_reopen() {
        let (log, dir) = temp_log();
        assert_eq!(log.append("a", "x", json!({})).unwrap().seq, 1);
        assert_eq!(log.append("a", "x", json!({})).unwrap().seq, 2);
        assert_eq!(log.append("b", "x", json!({})).unwrap().seq, 1);
        drop(log);
        let log = Log::open(&dir.join("events.db")).unwrap();
        assert_eq!(log.append("a", "x", json!({"k": 1})).unwrap().seq, 3);
        let all = log.read("a", 2, 10).unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(all[1].data["k"], 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unfinished_and_last_of() {
        let (log, dir) = temp_log();
        log.append("a", "machine.created", json!({})).unwrap();
        log.append("a", "exec.started", json!({})).unwrap();
        log.append("b", "machine.created", json!({})).unwrap();
        log.append("b", "machine.destroyed", json!({"reason": "r"}))
            .unwrap();
        assert_eq!(log.unfinished().unwrap(), ["a"]);
        assert_eq!(log.last_of("a", "machine.").unwrap().unwrap().seq, 1);
        assert_eq!(
            log.last_of("b", "machine.").unwrap().unwrap().str("reason"),
            Some("r")
        );
        assert!(log.last_of("c", "machine.").unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn follow_replays_then_goes_live_and_ends() {
        let (log, dir) = temp_log();
        let log = Arc::new(log);
        for _ in 0..300 {
            log.append("a", "exec.stdout", json!({})).unwrap();
        }
        log.append("b", "exec.stdout", json!({})).unwrap();
        let mut rx = follow(
            Arc::clone(&log),
            "a".into(),
            5,
            |_| true,
            |e| e.kind == "machine.destroyed",
        );
        let writer = Arc::clone(&log);
        tokio::task::spawn_blocking(move || {
            for _ in 0..2000 {
                writer.append("a", "exec.stdout", json!({})).unwrap();
            }
            writer.append("a", "machine.destroyed", json!({})).unwrap();
        });
        let mut seqs = Vec::new();
        while let Some(Ok(line)) = rx.recv().await {
            let v: Value = serde_json::from_slice(&line).unwrap();
            assert_eq!(v["machine"], "a");
            seqs.push(v["seq"].as_u64().unwrap());
        }
        assert_eq!(seqs, (5..=2301).collect::<Vec<_>>());

        // Past the end of a finished machine: the stream ends at once.
        let mut rx = follow(Arc::clone(&log), "a".into(), 9999, |_| true, |_| false);
        let ended = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(ended, Ok(None)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
