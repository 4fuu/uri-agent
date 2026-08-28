use crate::text_metrics::{count_visible_units, visible_units};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const STREAM_RATE_WINDOW: Duration = Duration::from_secs(3);
const DEFAULT_VISIBLE_UNITS_PER_SECOND: f64 = 100.0;
const DISPLAY_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const DISPLAY_SMOOTHING_TIME: Duration = Duration::from_secs(3);
const MIN_DISPLAY_RATE: f64 = 0.05;

#[derive(Default)]
struct StreamRateEstimator {
    first_output_at: Option<Instant>,
    total_units: usize,
    samples: VecDeque<(Instant, usize)>,
    in_visible_word: bool,
}

impl StreamRateEstimator {
    fn observe(&mut self, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        let (units, in_visible_word) = count_visible_units(text, self.in_visible_word);
        self.in_visible_word = in_visible_word;
        self.first_output_at.get_or_insert(now);
        self.total_units = self.total_units.saturating_add(units);
        self.samples.push_back((now, self.total_units));
        self.trim(now);
    }

    fn current_units_per_second(&mut self, now: Instant) -> Option<f64> {
        let first_output_at = self.first_output_at?;
        self.trim(now);
        if self.samples.len() < 2 {
            let elapsed = now.saturating_duration_since(first_output_at);
            if self.samples.back().is_some_and(|(sampled_at, _)| {
                now.saturating_duration_since(*sampled_at) >= STREAM_RATE_WINDOW
            }) {
                return Some(0.0);
            }
            let seconds = elapsed.as_secs_f64();
            return (seconds > 0.0)
                .then(|| DEFAULT_VISIBLE_UNITS_PER_SECOND.min(self.total_units as f64 / seconds));
        }
        let window_start = now
            .checked_sub(STREAM_RATE_WINDOW)
            .unwrap_or(first_output_at);
        let start = first_output_at.max(window_start);
        let baseline = if start <= first_output_at {
            0
        } else {
            self.samples
                .iter()
                .take_while(|(sampled_at, _)| *sampled_at <= start)
                .map(|(_, units)| *units)
                .last()
                .unwrap_or_default()
        };
        let seconds = now.saturating_duration_since(start).as_secs_f64();
        (seconds > 0.0).then(|| self.total_units.saturating_sub(baseline) as f64 / seconds)
    }

    fn elapsed(&self, completed_at: Instant) -> Option<Duration> {
        self.first_output_at
            .map(|started_at| completed_at.saturating_duration_since(started_at))
            .filter(|elapsed| !elapsed.is_zero())
    }

    fn trim(&mut self, now: Instant) {
        let boundary = now.checked_sub(STREAM_RATE_WINDOW).unwrap_or(now);
        while self.samples.len() > 1 && self.samples.get(1).is_some_and(|(at, _)| *at <= boundary) {
            self.samples.pop_front();
        }
    }
}

#[derive(Default)]
struct ThreadTokenRatio {
    visible_units: u64,
    output_tokens: u64,
}

impl ThreadTokenRatio {
    fn observe_response(&mut self, visible_units: usize, output_tokens: u64) {
        if visible_units == 0 || output_tokens == 0 {
            return;
        }
        self.visible_units = self.visible_units.saturating_add(visible_units as u64);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens);
    }

    fn token_rate(&self, units_per_second: Option<f64>) -> Option<f64> {
        let units_per_second = units_per_second.filter(|rate| *rate > 0.0)?;
        if self.visible_units == 0 || self.output_tokens == 0 {
            return None;
        }
        let visible_units_per_token = self.visible_units as f64 / self.output_tokens as f64;
        (visible_units_per_token > 0.0).then(|| units_per_second / visible_units_per_token)
    }
}

#[derive(Default)]
struct ResponseObservation {
    visible_units: usize,
    reasoning_visible: bool,
    usage: Option<ResponseUsage>,
}

struct ResponseUsage {
    output_tokens: u64,
    reasoning_tokens: u64,
    generation_time: Option<Duration>,
}

#[derive(Default)]
pub(super) struct TokenRateState {
    ratio: ThreadTokenRatio,
    stream: StreamRateEstimator,
    response: ResponseObservation,
    displayed_rate: Option<f64>,
    last_display_update_at: Option<Instant>,
    frozen_rate: Option<f64>,
    turn_active: bool,
    turn_complete: bool,
    turn_output_tokens: u64,
    turn_generation_time: Duration,
    final_average: Option<f64>,
}

impl TokenRateState {
    pub(super) fn restore_calibration(&mut self, visible_units: u64, output_tokens: u64) {
        self.ratio = ThreadTokenRatio {
            visible_units,
            output_tokens,
        };
    }

    pub(super) fn start_turn(&mut self) {
        self.stream = StreamRateEstimator::default();
        self.response = ResponseObservation::default();
        self.displayed_rate = None;
        self.last_display_update_at = None;
        self.frozen_rate = None;
        self.turn_active = true;
        self.turn_complete = true;
        self.turn_output_tokens = 0;
        self.turn_generation_time = Duration::ZERO;
        self.final_average = None;
    }

    pub(super) fn ensure_turn(&mut self) {
        if !self.turn_active {
            self.start_turn();
        }
    }

    pub(super) fn observe_stream_text(&mut self, text: &str, reasoning: bool, now: Instant) {
        if text.is_empty() {
            return;
        }
        self.ensure_turn();
        if self.frozen_rate.take().is_some() {
            self.last_display_update_at = Some(now);
        }
        self.response.reasoning_visible |= reasoning;
        self.stream.observe(text, now);
    }

    pub(super) fn observe_response_text(&mut self, text: &str, reasoning: bool) {
        self.response.visible_units = self
            .response
            .visible_units
            .saturating_add(visible_units(text));
        self.response.reasoning_visible |= reasoning && !text.is_empty();
    }

    pub(super) fn observe_usage(
        &mut self,
        output_tokens: u64,
        reasoning_tokens: u64,
        completed_at: Instant,
    ) {
        self.ensure_turn();
        self.response.usage = Some(ResponseUsage {
            output_tokens,
            reasoning_tokens,
            generation_time: self.stream.elapsed(completed_at),
        });
    }

    pub(super) fn finish_response(&mut self, now: Instant) {
        let usage = self.response.usage.take();
        let output_tokens = usage.as_ref().map(|usage| {
            if self.response.reasoning_visible {
                usage.output_tokens
            } else {
                usage.output_tokens.saturating_sub(usage.reasoning_tokens)
            }
        });
        if let Some(output_tokens) = output_tokens {
            self.ratio
                .observe_response(self.response.visible_units, output_tokens);
        }

        match (usage, output_tokens) {
            (Some(usage), Some(output_tokens))
                if output_tokens > 0 && usage.generation_time.is_some() =>
            {
                self.turn_output_tokens = self.turn_output_tokens.saturating_add(output_tokens);
                self.turn_generation_time = self
                    .turn_generation_time
                    .saturating_add(usage.generation_time.unwrap_or_default());
            }
            _ => self.turn_complete = false,
        }

        self.frozen_rate = self.display_rate(now);
        self.stream = StreamRateEstimator::default();
        self.response = ResponseObservation::default();
    }

    pub(super) fn retry_response(&mut self) {
        self.stream = StreamRateEstimator::default();
        self.response = ResponseObservation::default();
        self.displayed_rate = None;
        self.last_display_update_at = None;
        self.frozen_rate = None;
    }

    pub(super) fn finish_turn(&mut self) {
        self.final_average = (self.turn_active
            && self.turn_complete
            && self.turn_output_tokens > 0
            && !self.turn_generation_time.is_zero())
        .then(|| self.turn_output_tokens as f64 / self.turn_generation_time.as_secs_f64());
        self.turn_active = false;
        self.stream = StreamRateEstimator::default();
        self.response = ResponseObservation::default();
        self.displayed_rate = None;
        self.last_display_update_at = None;
        self.frozen_rate = None;
    }

    pub(super) fn fail_turn(&mut self) {
        self.finish_turn();
        self.final_average = None;
    }

    pub(super) fn finish_hydration(&mut self) {
        self.turn_active = false;
        self.turn_complete = false;
        self.turn_output_tokens = 0;
        self.turn_generation_time = Duration::ZERO;
        self.final_average = None;
        self.stream = StreamRateEstimator::default();
        self.response = ResponseObservation::default();
        self.displayed_rate = None;
        self.last_display_update_at = None;
        self.frozen_rate = None;
    }

    pub(super) fn clear_final(&mut self) {
        self.final_average = None;
    }

    pub(super) fn display_rate(&mut self, now: Instant) -> Option<f64> {
        if let Some(rate) = self.frozen_rate {
            return Some(rate);
        }
        if self.last_display_update_at.is_some_and(|updated_at| {
            now.saturating_duration_since(updated_at) < DISPLAY_UPDATE_INTERVAL
        }) {
            return self.displayed_rate;
        }
        let instant_rate = self
            .ratio
            .token_rate(self.stream.current_units_per_second(now))
            .filter(|rate| *rate >= MIN_DISPLAY_RATE);
        let displayed = match (
            self.displayed_rate,
            instant_rate,
            self.last_display_update_at,
        ) {
            (_, None, _) => None,
            (Some(previous), Some(instant), Some(updated_at)) => {
                let elapsed = now.saturating_duration_since(updated_at).as_secs_f64();
                let alpha = 1.0 - (-elapsed / DISPLAY_SMOOTHING_TIME.as_secs_f64()).exp();
                Some(previous + alpha * (instant - previous))
            }
            (_, Some(instant), _) => Some(instant),
        };
        self.displayed_rate = displayed;
        self.last_display_update_at = Some(now);
        displayed
    }

    pub(super) fn final_average(&self) -> Option<f64> {
        self.final_average
    }
}

pub(super) fn format_token_rate(rate: f64) -> String {
    if rate >= 100.0 {
        format!("{rate:.0} tok/s")
    } else if rate >= 10.0 {
        format!("{rate:.1} tok/s")
    } else {
        format!("{rate:.2} tok/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_units_follow_words_cjk_punctuation_and_chunk_boundaries() {
        assert_eq!(visible_units("hello, 世界! e\u{301}"), 6);
        let (first, in_word) = count_visible_units("hel", false);
        let (second, in_word) = count_visible_units("lo 世", in_word);
        let (third, _) = count_visible_units("界", in_word);
        assert_eq!((first, second, third), (1, 1, 1));
    }

    #[test]
    fn estimator_uses_a_three_second_window_and_single_sample_cap() {
        let start = Instant::now();
        let mut estimator = StreamRateEstimator::default();
        estimator.observe("one two", start);
        assert_eq!(
            estimator.current_units_per_second(start + Duration::from_millis(10)),
            Some(100.0)
        );
        estimator.observe(" three four", start + Duration::from_secs(1));
        estimator.observe(" five six", start + Duration::from_secs(4));
        assert_eq!(
            estimator.current_units_per_second(start + Duration::from_secs(4)),
            Some(2.0 / 3.0)
        );
    }

    #[test]
    fn final_average_sums_model_time_and_excludes_hidden_reasoning() {
        let start = Instant::now();
        let mut state = TokenRateState::default();
        state.start_turn();
        state.observe_stream_text("answer", false, start);
        state.observe_usage(30, 10, start + Duration::from_secs(2));
        state.observe_response_text("answer", false);
        state.finish_response(start + Duration::from_secs(2));
        state.observe_stream_text("done", false, start + Duration::from_secs(5));
        state.observe_usage(20, 0, start + Duration::from_secs(7));
        state.observe_response_text("done", false);
        state.finish_response(start + Duration::from_secs(7));
        state.finish_turn();
        assert_eq!(state.final_average(), Some(10.0));
    }

    #[test]
    fn missing_response_metrics_suppress_a_partial_turn_average() {
        let start = Instant::now();
        let mut state = TokenRateState::default();
        state.start_turn();
        state.observe_stream_text("measured", false, start);
        state.observe_usage(20, 0, start + Duration::from_secs(2));
        state.observe_response_text("measured", false);
        state.finish_response(start + Duration::from_secs(2));
        state.observe_response_text("unmeasured", false);
        state.finish_response(start + Duration::from_secs(3));
        state.finish_turn();
        assert_eq!(state.final_average(), None);
    }

    #[test]
    fn failed_stream_time_is_discarded_before_a_retry() {
        let start = Instant::now();
        let mut state = TokenRateState::default();
        state.start_turn();
        state.observe_stream_text("discarded", false, start);
        state.retry_response();
        state.observe_stream_text("recovered", false, start + Duration::from_secs(5));
        state.observe_usage(10, 0, start + Duration::from_secs(7));
        state.observe_response_text("recovered", false);
        state.finish_response(start + Duration::from_secs(7));
        state.finish_turn();
        assert_eq!(state.final_average(), Some(5.0));
    }
}
