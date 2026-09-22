//! Network work, off the UI thread.
//!
//! Upstream history and per-release changelogs can take seconds to fetch the
//! first time. The dashboard keeps drawing — and animating — while a worker
//! thread fetches, and picks results up as they arrive.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use chrono::Utc;

use crate::config::{ComponentConfig, LineageConfig};
use crate::timeline::{self, Relevance, Shape, Upstream};

pub enum Request {
    Upstream {
        component: String,
        cfg: ComponentConfig,
        keep: BTreeSet<String>,
    },
    Shape {
        component: String,
        cfg: ComponentConfig,
        version: String,
    },
    /// The last journal lines of one boot.
    Tail { boot_id: String, journalctl: String },
}

pub enum Response {
    Upstream {
        component: String,
        upstream: Upstream,
    },
    Shape {
        component: String,
        version: String,
        shape: Option<Shape>,
    },
    Tail {
        boot_id: String,
        lines: Vec<String>,
    },
}

pub struct Worker {
    pub requests: Sender<Request>,
    pub responses: Receiver<Response>,
}

impl Worker {
    pub fn spawn(lineage: LineageConfig, cache_dir: PathBuf, relevance: Relevance) -> Self {
        let (req_tx, req_rx) = channel::<Request>();
        let (resp_tx, resp_rx) = channel::<Response>();
        std::thread::Builder::new()
            .name("sluice-fetch".into())
            .spawn(move || {
                for request in req_rx {
                    let response = match request {
                        Request::Upstream {
                            component,
                            cfg,
                            keep,
                        } => Response::Upstream {
                            upstream: timeline::fetch_upstream(
                                &lineage,
                                &cache_dir,
                                &cfg,
                                &keep,
                                Utc::now().date_naive(),
                            ),
                            component,
                        },
                        Request::Shape {
                            component,
                            cfg,
                            version,
                        } => Response::Shape {
                            shape: timeline::fetch_shape(
                                &lineage, &cache_dir, &cfg, &version, &relevance,
                            ),
                            component,
                            version,
                        },
                        Request::Tail {
                            boot_id,
                            journalctl,
                        } => Response::Tail {
                            lines: super::boots::journal_tail(&journalctl, &boot_id, 40),
                            boot_id,
                        },
                    };
                    if resp_tx.send(response).is_err() {
                        break;
                    }
                }
            })
            .expect("spawning the fetch thread");
        Worker {
            requests: req_tx,
            responses: resp_rx,
        }
    }
}
