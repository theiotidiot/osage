//! Fan-in of terminal input and database events onto a single channel.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crossterm::event::Event as TermEvent;

use crate::db::DbEvent;

#[derive(Debug)]
pub enum AppEvent {
    Input(TermEvent),
    Db(DbEvent),
    /// Periodic redraw, so spinners animate while nothing else happens.
    Tick,
}

/// Spawn the reader threads and return the merged receiver.
///
/// `db_events` is drained by a forwarding thread so the main loop only ever
/// blocks on one channel.
pub fn spawn(db_events: Receiver<DbEvent>, tick: Duration) -> Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel();

    spawn_input(tx.clone());
    spawn_forwarder(db_events, tx.clone());
    spawn_ticker(tx, tick);

    rx
}

fn spawn_input(tx: Sender<AppEvent>) {
    std::thread::Builder::new()
        .name("osage-input".into())
        .spawn(move || {
            while let Ok(ev) = crossterm::event::read() {
                if tx.send(AppEvent::Input(ev)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn input thread");
}

fn spawn_forwarder(db_events: Receiver<DbEvent>, tx: Sender<AppEvent>) {
    std::thread::Builder::new()
        .name("osage-db-events".into())
        .spawn(move || {
            while let Ok(ev) = db_events.recv() {
                if tx.send(AppEvent::Db(ev)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn db event forwarder");
}

fn spawn_ticker(tx: Sender<AppEvent>, tick: Duration) {
    std::thread::Builder::new()
        .name("osage-tick".into())
        .spawn(move || {
            loop {
                std::thread::sleep(tick);
                if tx.send(AppEvent::Tick).is_err() {
                    break;
                }
            }
        })
        .expect("spawn ticker");
}
