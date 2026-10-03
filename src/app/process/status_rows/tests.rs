use std::{
    io,
    sync::{Arc, Mutex},
};

use indicatif::TermLike;

use super::*;

#[derive(Clone, Debug, Default)]
struct RecordingTerm {
    clears: Arc<Mutex<usize>>,
    writes: Arc<Mutex<usize>>,
}

impl RecordingTerm {
    fn clear_count(&self) -> usize {
        *self.clears.lock().expect("recording term lock")
    }

    fn reset(&self) {
        *self.clears.lock().expect("recording term lock") = 0;
        *self.writes.lock().expect("recording term lock") = 0;
    }

    fn write_count(&self) -> usize {
        *self.writes.lock().expect("recording term lock")
    }
}

impl TermLike for RecordingTerm {
    fn width(&self) -> u16 {
        120
    }

    fn move_cursor_up(&self, _: usize) -> io::Result<()> {
        Ok(())
    }

    fn move_cursor_down(&self, _: usize) -> io::Result<()> {
        Ok(())
    }

    fn move_cursor_right(&self, _: usize) -> io::Result<()> {
        Ok(())
    }

    fn move_cursor_left(&self, _: usize) -> io::Result<()> {
        Ok(())
    }

    fn write_line(&self, _: &str) -> io::Result<()> {
        let mut writes = self
            .writes
            .lock()
            .map_err(|err| io::Error::other(err.to_string()))?;
        *writes += 1;
        Ok(())
    }

    fn write_str(&self, _: &str) -> io::Result<()> {
        let mut writes = self
            .writes
            .lock()
            .map_err(|err| io::Error::other(err.to_string()))?;
        *writes += 1;
        Ok(())
    }

    fn clear_line(&self) -> io::Result<()> {
        let mut clears = self
            .clears
            .lock()
            .map_err(|err| io::Error::other(err.to_string()))?;
        *clears += 1;
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn status_rows_reuse_their_terminal_lines_after_a_pr_finishes() {
    let term = RecordingTerm::default();
    let statuses = PrStatusRows::with_draw_target(
        [
            ("example/repo".to_string(), 1),
            ("example/repo".to_string(), 2),
        ],
        ProgressDrawTarget::term_like(Box::new(term.clone())),
    );

    term.reset();
    statuses.finish_success("example/repo", 1, "Approved and merged");
    statuses.update("example/repo", 2, "Approving pull request");

    assert!(
        !statuses
            .bars
            .get(&("example/repo".to_string(), 1))
            .expect("first status bar")
            .is_finished(),
        "completed rows should remain in the status board"
    );
    assert_eq!(term.clear_count(), 0, "status rows should not be appended");
}

#[test]
fn status_rows_do_not_redraw_after_processing_finishes() {
    let term = RecordingTerm::default();
    let statuses = PrStatusRows::with_draw_target(
        [
            ("example/repo".to_string(), 1),
            ("example/repo".to_string(), 2),
        ],
        ProgressDrawTarget::term_like(Box::new(term.clone())),
    );

    statuses.finish_success("example/repo", 1, "Approved and merged");
    statuses.finish_success("example/repo", 2, "Approved and merged");
    term.reset();
    statuses.finish();
    let writes_before_drop = term.write_count();
    drop(statuses);

    assert_eq!(
        term.write_count(),
        writes_before_drop,
        "dropping status rows must not redraw over the final application output"
    );
}

#[test]
fn status_rows_stop_when_processing_returns_an_error() {
    let statuses = PrStatusRows::with_draw_target(
        [("example/repo".to_string(), 1)],
        ProgressDrawTarget::hidden(),
    );
    let bar = statuses.bars.values().next().expect("status bar").clone();
    let process = || -> Result<(), &'static str> {
        let _statuses = statuses;
        Err("Pull Request has merge conflicts")?;
        Ok(())
    };

    assert!(process().is_err());
    assert!(bar.is_finished(), "error returns must stop status rows");
}

#[test]
fn status_rows_are_disabled_for_verbose_runs() {
    assert!(!should_render_status_rows(false, true));
    assert!(!should_render_status_rows(true, false));
    assert!(should_render_status_rows(false, false));
}
