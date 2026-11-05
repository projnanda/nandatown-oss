//! Observer recorder shared by the `nasiko-hitl` integration tests that assert what the host is told.

use uuid::Uuid;

/// Records what `nasiko_hitl::observer` is told. The observer is process-wide (one per test
/// binary), so tests share this recorder and look only at their own row ids.
#[allow(dead_code)]
#[derive(Default)]
pub struct Recorder {
    pauses: std::sync::Mutex<Vec<Uuid>>,
    outcomes: std::sync::Mutex<Vec<(Uuid, nasiko_hitl::ResumeStatus)>>,
}

#[allow(dead_code)]
impl Recorder {
    /// The recorder installed as this binary's observer, installing it on first use.
    pub fn installed() -> &'static Recorder {
        static RECORDER: std::sync::OnceLock<std::sync::Arc<Recorder>> = std::sync::OnceLock::new();
        RECORDER.get_or_init(|| {
            let recorder = std::sync::Arc::new(Recorder::default());
            assert!(nasiko_hitl::install_observer(recorder.clone()));
            recorder
        })
    }

    /// How many times `id` was reported as a new pause.
    pub fn pauses_of(&self, id: Uuid) -> usize {
        self.pauses
            .lock()
            .unwrap()
            .iter()
            .filter(|p| **p == id)
            .count()
    }

    /// Every outcome reported for `id`, in order.
    pub fn outcomes_of(&self, id: Uuid) -> Vec<nasiko_hitl::ResumeStatus> {
        self.outcomes
            .lock()
            .unwrap()
            .iter()
            .filter(|(row, _)| *row == id)
            .map(|(_, outcome)| *outcome)
            .collect()
    }
}

impl nasiko_hitl::HitlObserver for Recorder {
    fn pause_recorded(&self, row: &nasiko_hitl::HitlRequest) {
        self.pauses.lock().unwrap().push(row.id);
    }

    fn resume_finished(
        &self,
        row: &nasiko_hitl::HitlRequest,
        outcome: nasiko_hitl::ResumeStatus,
        _finished_at: chrono::DateTime<chrono::Utc>,
    ) {
        self.outcomes.lock().unwrap().push((row.id, outcome));
    }
}
