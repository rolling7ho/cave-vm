//! Progress for the request this thread is handling, such as the first
//! image download during `start` (design, "First run does everything").
//! The server sets a sink for the request; code deep in a request, such as
//! the image fetch, reports to it without a callback threaded through every
//! layer. With no sink set, reports go nowhere.

use std::cell::RefCell;

use cave_vm_proto::daemon::Reply;

type Sink = Box<dyn FnMut(Reply)>;

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Runs `f` with `sink` receiving this thread's progress messages.
pub fn with_sink<T>(sink: impl FnMut(Reply) + 'static, f: impl FnOnce() -> T) -> T {
    let old = SINK.with(|s| s.replace(Some(Box::new(sink))));
    let out = f();
    SINK.with(|s| *s.borrow_mut() = old);
    out
}

/// Sends a progress message to this thread's sink, if any.
pub fn report(message: impl Into<String>, done: Option<u64>, total: Option<u64>) {
    SINK.with(|s| {
        if let Some(sink) = s.borrow_mut().as_mut() {
            sink(Reply::Progress {
                message: message.into(),
                done,
                total,
            });
        }
    });
}

/// Reports a download in steps of a tenth, so a large file sends about
/// ten messages: `downloading <what>: 40% of 1.9 GB`.
pub struct Download {
    what: String,
    reported: Option<u64>,
}

impl Download {
    pub fn new(what: impl Into<String>) -> Download {
        Download {
            what: what.into(),
            reported: None,
        }
    }

    /// Called with the bytes done and the total, if known.
    pub fn update(&mut self, done: u64, total: Option<u64>) {
        let Some(total) = total.filter(|&t| t > 0) else {
            if self.reported.is_none() {
                self.reported = Some(0);
                report(format!("downloading {}", self.what), Some(done), None);
            }
            return;
        };
        let tenth = (done.min(total) * 10 / total) * 10;
        if self.reported.is_some_and(|r| r >= tenth) {
            return;
        }
        self.reported = Some(tenth);
        report(
            format!(
                "downloading {}: {tenth}% of {:.1} GB",
                self.what,
                total as f64 / (1u64 << 30) as f64
            ),
            Some(done),
            Some(total),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    fn messages(f: impl FnOnce()) -> Vec<String> {
        let got = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&got);
        with_sink(
            move |reply| {
                if let Reply::Progress { message, .. } = reply {
                    sink.borrow_mut().push(message);
                }
            },
            f,
        );
        got.take()
    }

    #[test]
    fn a_download_reports_each_tenth_once() {
        let got = messages(|| {
            let mut d = Download::new("the debian image");
            for done in (0..=1000).step_by(7) {
                d.update(done, Some(1000));
            }
            d.update(1000, Some(1000));
        });
        assert_eq!(got.len(), 11, "{got:?}");
        assert_eq!(got[0], "downloading the debian image: 0% of 0.0 GB");
        assert!(got[10].contains(": 100% of"), "{got:?}");
    }

    #[test]
    fn without_a_size_it_reports_once() {
        let got = messages(|| {
            let mut d = Download::new("the kernel");
            d.update(0, None);
            d.update(500, None);
        });
        assert_eq!(got, ["downloading the kernel"]);
    }

    #[test]
    fn with_no_sink_reports_go_nowhere_and_sinks_nest() {
        report("nobody hears this", None, None);
        let outer = messages(|| {
            report("outer", None, None);
            let inner = messages(|| report("inner", None, None));
            assert_eq!(inner, ["inner"]);
            report("outer again", None, None);
        });
        assert_eq!(outer, ["outer", "outer again"]);
    }
}
