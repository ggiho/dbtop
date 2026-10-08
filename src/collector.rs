use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::Backend;
use crate::model::{ExplainTarget, Meters, Plan, PlanKey, Sample};

pub enum Req {
    Explain(ExplainTarget),
}

pub enum Msg {
    Poll {
        sample: Sample,
        /// The numbers to chart, named as in `sample.metrics`; `None` where there is none yet.
        values: Vec<(&'static str, Option<f64>)>,
    },
    PollFailed(String),
    Plan {
        key: PlanKey,
        result: Result<Plan, String>,
    },
}

/// Polls `backend` every `interval` on its own thread so a slow database never freezes the UI.
/// The thread ends when the returned `Sender` is dropped.
pub fn spawn(mut backend: Box<dyn Backend>, interval: Duration) -> (Receiver<Msg>, Sender<Req>) {
    let (msg_tx, msg_rx) = mpsc::channel();
    let (req_tx, req_rx) = mpsc::channel();

    thread::spawn(move || {
        let mut meters = Meters::default();
        loop {
            let msg = match backend.poll() {
                Ok(sample) => {
                    let values = meters.read(Instant::now(), &sample.metrics);
                    Msg::Poll { sample, values }
                }
                Err(e) => Msg::PollFailed(format!("{e:#}")),
            };
            if msg_tx.send(msg).is_err() {
                return;
            }

            // Wait for the next poll, answering plan requests in the meantime.
            let next = Instant::now() + interval;
            while let Some(wait) = next.checked_duration_since(Instant::now()) {
                match req_rx.recv_timeout(wait) {
                    Ok(Req::Explain(target)) => {
                        let result = backend.explain(&target).map_err(|e| format!("{e:#}"));
                        if msg_tx
                            .send(Msg::Plan {
                                key: target.key(),
                                result,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });

    (msg_rx, req_tx)
}
