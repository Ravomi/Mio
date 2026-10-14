//! Every run-protocol path that existed at 0.42.0 still resolves after the
//! protocol crate was dissolved (everything an agent loop is — program, spec,
//! request preparation, response, error, decision data, assembler, loop-side
//! transcript helpers — is `mio_agent::run`; mio-core keeps the message-model
//! invariants in `mio_core::transcript`).
//! Compile-only: a missing path fails the build.

#![allow(unused_imports)]

use mio_agent::agent::hook::{
    InvalidToolCallAction, InvalidToolCallContext, RequestPatch, RetryRequest, RunEntry,
};
use mio_agent::agent::run::output_mode::OutputMode as _OutputModeViaRun;
use mio_agent::agent::run::streamed::{
    PartialStreamedTurn as _P, StreamedInvalidToolCall as _I, StreamedResolution as _R,
    StreamedTurn as _T, StreamedTurnAssembler as _A, StreamedTurnEvent as _E,
};
use mio_agent::agent::run::{
    AgentRun as _AgentRunViaRun, AgentRunStep as _StepViaRun, ModelTurn as _TurnViaRun,
    ModelTurnOutcome as _OutcomeViaRun, PendingToolCall as _PendingViaRun,
    RunEntry as _EntryViaRun, TurnTools as _ToolsViaRun,
};
use mio_agent::agent::{
    AgentRun, AgentRunStep, CompletionCall, ModelTurn, ModelTurnOutcome, OutputMode,
    PendingToolCall, PromptResponse, RunSpec, TurnTools,
};
use mio_agent::completion::PromptError;
use mio_agent::run::policy::InvalidToolCallAction as _ActionDirect;
use mio_agent::run::response::{
    CompletionCall as _CallDirect, PromptError as _ErrDirect, PromptResponse as _RespDirect,
};
use mio_agent::run::streamed::StreamedTurnAssembler as _AssemblerDirect;
use mio_agent::run::{AgentRun as _AgentRunDirect, RunEntry as _EntryDirect};
use mio_agent::run::{
    output::OutputMode as _OutputModeDirect,
    patch::RequestPatch as _PatchDirect,
    prepare::{PreparedRequest, prepare_request},
    spec::RunSpec as _SpecDirect,
    transcript::{TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER as _Peer, build_full_history as _Full},
};
use mio_core::transcript::{
    TranscriptError, tool_result_output as _ToolResult, validate_canonical,
};

#[test]
fn run_protocol_paths_resolve() {
    let _ = AgentRun::new("x").max_turns(1);
    let _ = RunSpec::new();
    let _: fn(&[mio_core::message::Message]) -> Result<(), TranscriptError> = validate_canonical;
}
