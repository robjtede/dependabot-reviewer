//! Render and stop the terminal status rows for a batch of pull requests.

use std::{collections::HashMap, io::IsTerminal as _, time::Duration};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

pub(super) struct PrStatusRows {
    _multi_progress: MultiProgress,
    bars: HashMap<(String, u64), ProgressBar>,
}

impl PrStatusRows {
    pub(super) fn new(prs: impl IntoIterator<Item = (String, u64)>) -> Option<Self> {
        if !std::io::stdout().is_terminal() {
            return None;
        }

        Some(Self::with_draw_target(prs, ProgressDrawTarget::stdout()))
    }

    fn with_draw_target(
        prs: impl IntoIterator<Item = (String, u64)>,
        draw_target: ProgressDrawTarget,
    ) -> Self {
        let multi_progress = MultiProgress::with_draw_target(draw_target);
        multi_progress.set_move_cursor(true);
        let style = ProgressStyle::with_template("  {spinner:.dim} {msg}")
            .expect("progress status template is valid");
        let mut bars = HashMap::new();

        for (repo, pr_number) in prs {
            let bar = multi_progress.add(ProgressBar::new_spinner());
            bar.set_style(style.clone());
            bar.enable_steady_tick(Duration::from_millis(120));
            bar.set_message(Self::message(&repo, pr_number, "Waiting to process"));
            bars.insert((repo, pr_number), bar);
        }

        Self {
            _multi_progress: multi_progress,
            bars,
        }
    }

    pub(super) fn update(&self, repo: &str, pr_number: u64, status: &str) {
        if let Some(bar) = self.bars.get(&(repo.to_owned(), pr_number)) {
            bar.set_message(Self::message(repo, pr_number, status));
        }
    }

    pub(super) fn finish_success(&self, repo: &str, pr_number: u64, status: &str) {
        self.complete(repo, pr_number, &format!("✓ {status}"));
    }

    pub(super) fn finish_skipped(&self, repo: &str, pr_number: u64, status: &str) {
        self.complete(repo, pr_number, &format!("⊘ {status}"));
    }

    pub(super) fn complete(&self, repo: &str, pr_number: u64, status: &str) {
        if let Some(bar) = self.bars.get(&(repo.to_owned(), pr_number)) {
            bar.disable_steady_tick();
            bar.set_style(
                ProgressStyle::with_template("  {msg}")
                    .expect("completed status template is valid"),
            );
            bar.set_message(Self::message(repo, pr_number, status));
        }
    }

    pub(super) fn finish(&self) {
        for bar in self.bars.values() {
            bar.finish();
        }
        println!();
    }

    pub(super) fn suspend<T>(&self, callback: impl FnOnce() -> T) -> T {
        self._multi_progress.suspend(callback)
    }

    fn message(repo: &str, pr_number: u64, status: &str) -> String {
        format!("{repo}#{pr_number}: {status}")
    }
}

impl Drop for PrStatusRows {
    fn drop(&mut self) {
        for bar in self.bars.values() {
            bar.disable_steady_tick();
            if !bar.is_finished() {
                bar.abandon();
            }
        }
    }
}

pub(super) fn should_render_status_rows(dry_run: bool, verbose: bool) -> bool {
    !dry_run && !verbose
}

#[cfg(test)]
mod tests;
