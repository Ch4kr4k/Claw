//! Per-step speed figures for `/stats` (after deepseek-harness's stats
//! view): for each model request, the context it carried and how much of it
//! the server had cached, the wait for the first token, the time spent
//! generating and the output speed, and the time the step's tools took.
//! Mostly for tuning a local server: a low cache share means the prompt is
//! re-read every step, a long first-token wait means slow prompt reading.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use runtime::TokenUsage;

const KEEP_STEPS: usize = 500;

#[derive(Debug, Clone)]
struct Step {
    turn: usize,
    model: String,
    /// Prompt tokens, and how many of them came from the cache.
    context: u32,
    cached: u32,
    output: u32,
    first_token: Option<Duration>,
    total: Duration,
    tools: Duration,
}

#[derive(Debug, Default)]
struct State {
    turn: usize,
    steps: Vec<Step>,
    started: Option<Instant>,
    first_token: Option<Duration>,
}

static STATE: Mutex<State> = Mutex::new(State {
    turn: 0,
    steps: Vec::new(),
    started: None,
    first_token: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A new user turn starts.
pub(crate) fn begin_turn() {
    state().turn += 1;
}

/// A model request is sent.
pub(crate) fn begin_step() {
    let mut state = state();
    state.started = Some(Instant::now());
    state.first_token = None;
}

/// Something of the reply arrived (only the first call counts).
pub(crate) fn mark_first_token() {
    let mut state = state();
    if state.first_token.is_none() {
        state.first_token = state.started.map(|started| started.elapsed());
    }
}

/// The request finished, using `usage`.
pub(crate) fn end_step(model: &str, usage: TokenUsage) {
    let mut state = state();
    let Some(started) = state.started.take() else {
        return;
    };
    let step = Step {
        turn: state.turn,
        model: model.to_string(),
        context: prompt_tokens(&usage),
        cached: usage.cache_read_input_tokens,
        output: usage.output_tokens,
        first_token: state.first_token.take(),
        total: started.elapsed(),
        tools: Duration::ZERO,
    };
    state.steps.push(step);
    if state.steps.len() > KEEP_STEPS {
        state.steps.remove(0);
    }
}

/// The tools of the latest step took `elapsed`.
pub(crate) fn add_tool_time(elapsed: Duration) {
    if let Some(step) = state().steps.last_mut() {
        step.tools += elapsed;
    }
}

/// Adds the time until it is dropped to the latest step's tool time.
pub(crate) struct ToolTimer(Instant);

impl ToolTimer {
    pub(crate) fn start() -> Self {
        Self(Instant::now())
    }
}

impl Drop for ToolTimer {
    fn drop(&mut self) {
        add_tool_time(self.0.elapsed());
    }
}

fn prompt_tokens(usage: &TokenUsage) -> u32 {
    usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens
}

/// Output tokens per second of generation (the time after the first
/// token), when it can be told.
fn output_speed(step: &Step) -> Option<f64> {
    let generating = step.total.checked_sub(step.first_token?)?.as_secs_f64();
    (generating > 0.05 && step.output > 0).then(|| f64::from(step.output) / generating)
}

fn seconds(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}

/// The table `/stats` prints: the last turn's steps, then the session.
pub(crate) fn report() -> String {
    let state = state();
    if state.steps.is_empty() {
        return "Speed: no model requests yet in this session.".to_string();
    }
    let last_turn = state.steps.last().map_or(0, |step| step.turn);
    let mut lines = vec![
        format!(
            "Speed, last turn ({})",
            state.steps.last().map_or("", |step| step.model.as_str())
        ),
        "  step  context  cached  first token  model time  output  tok/s  tools".to_string(),
    ];
    for (index, step) in state
        .steps
        .iter()
        .filter(|step| step.turn == last_turn)
        .enumerate()
    {
        lines.push(format!(
            "  {:>4}  {:>7}  {:>5}%  {:>11}  {:>10}  {:>6}  {:>5}  {:>5}",
            index + 1,
            step.context,
            cache_share(step.cached, step.context),
            step.first_token.map_or("-".to_string(), seconds),
            seconds(step.total),
            step.output,
            output_speed(step).map_or("-".to_string(), |speed| format!("{speed:.1}")),
            seconds(step.tools),
        ));
    }
    let steps = &state.steps;
    let context: u64 = steps.iter().map(|step| u64::from(step.context)).sum();
    let cached: u64 = steps.iter().map(|step| u64::from(step.cached)).sum();
    let model_time: Duration = steps.iter().map(|step| step.total).sum();
    let tool_time: Duration = steps.iter().map(|step| step.tools).sum();
    let waits = steps
        .iter()
        .filter_map(|step| step.first_token)
        .collect::<Vec<_>>();
    let speeds = steps.iter().filter_map(output_speed).collect::<Vec<_>>();
    lines.push(format!(
        "Session: {} steps in {} turns · cache {}% of prompt tokens · first token {} on average · \
         {} tok/s output · model {} vs tools {}",
        steps.len(),
        state.turn,
        cache_share(
            u32::try_from(cached).unwrap_or(u32::MAX),
            u32::try_from(context).unwrap_or(u32::MAX)
        ),
        if waits.is_empty() {
            "-".to_string()
        } else {
            seconds(waits.iter().sum::<Duration>() / u32::try_from(waits.len()).unwrap_or(1))
        },
        if speeds.is_empty() {
            "-".to_string()
        } else {
            format!("{:.1}", speeds.iter().sum::<f64>() / speeds.len() as f64)
        },
        seconds(model_time),
        seconds(tool_time),
    ));
    lines.join("\n")
}

fn cache_share(cached: u32, total: u32) -> u32 {
    if total == 0 {
        0
    } else {
        u32::try_from(u64::from(cached) * 100 / u64::from(total)).unwrap_or(100)
    }
}

#[cfg(test)]
mod tests {
    use super::{output_speed, Step};
    use std::time::Duration;

    #[test]
    fn output_speed_counts_only_the_time_after_the_first_token() {
        let step = Step {
            turn: 1,
            model: "local/x".into(),
            context: 1000,
            cached: 900,
            output: 100,
            first_token: Some(Duration::from_secs(3)),
            total: Duration::from_secs(5),
            tools: Duration::ZERO,
        };
        assert_eq!(output_speed(&step), Some(50.0));
        assert_eq!(super::cache_share(900, 1000), 90);
    }
}
