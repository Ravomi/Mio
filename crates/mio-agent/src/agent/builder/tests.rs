use super::*;
use crate::test_utils::{MockAddTool, MockCompletionModel};

#[derive(Clone)]
struct BuilderHook;

impl AgentHook for BuilderHook {}

#[test]
fn hook_can_be_set_after_tool_configuration() {
    let _agent = AgentBuilder::new(MockCompletionModel::text("ok"))
        .tool(MockAddTool)
        .add_hook(BuilderHook)
        .build();
}

/// An agent over a host's bus holds no driver to tap: recording is the
/// host's, through its driver. Asking the builder for it is the host's
/// programming error, refused at build like a wrong-family host key —
/// never a debug assertion about generated keys, never a silent agent
/// that records nothing.
#[test]
#[should_panic(expected = "cannot record: the host records through its driver")]
fn recording_over_a_host_bus_is_refused_at_build() {
    let (dispatcher, registrar, _driver) = crate::bus::Bus::channel();
    let _agent = AgentBuilder::over_bus(
        dispatcher,
        registrar,
        "host",
        mio_core::effect::HandlerKey::from("model"),
    )
    .record_to(UnusedRecorder)
    .build();
}

mod conversation_without_memory {
    use super::*;
    use crate::test_utils::TraceCapture;

    /// The messages of every event at warn level or above.
    fn warnings(capture: &TraceCapture) -> Vec<String> {
        capture
            .events()
            .iter()
            .filter(|event| event.level <= tracing::Level::WARN)
            .map(|event| event.message())
            .collect()
    }

    /// A conversation id with no memory backend is not silently inert: the
    /// build warns once, naming the setter. With a backend it does not.
    #[tokio::test]
    async fn build_warns_when_conversation_has_no_memory_backend() {
        let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
        let capture = TraceCapture::default();
        let _default = tracing::subscriber::set_default(capture.subscriber());

        let _agent = AgentBuilder::new(MockCompletionModel::text("x"))
            .conversation("thread-1")
            .build();
        let seen = warnings(&capture);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("AgentBuilder::conversation"), "{seen:?}");

        capture.clear();
        let _agent = AgentBuilder::new(MockCompletionModel::text("x"))
            .memory(mio_core::memory::InMemoryConversationMemory::new())
            .conversation("thread-1")
            .build();
        assert!(warnings(&capture).is_empty());
    }
}

// Building over a host bus must reject recording before invoking the recorder.
struct UnusedRecorder;

impl mio_core::serve::Recorder for UnusedRecorder {
    fn handlers(&self, _: Vec<mio_core::effect::HandlerDescriptor>) {
        panic!("recorder must not be used")
    }
    fn begin(
        &self,
        _: mio_core::effect::EffectId,
        _: mio_core::effect::HandlerKey,
        _: mio_core::effect::EffectKind,
        _: mio_core::serve::Origin,
    ) {
        panic!("recorder must not be used")
    }
    fn discard(&self, _: mio_core::effect::EffectId) {
        panic!("recorder must not be used")
    }
    fn patch(&self, _: mio_core::effect::EffectId, _: mio_core::effect::EffectKind) {
        panic!("recorder must not be used")
    }
    fn keep_events(&self) -> bool {
        panic!("recorder must not be used")
    }
    fn event(
        &self,
        _: mio_core::effect::EffectId,
        _: &mio_core::streaming::Item<mio_core::streaming::StreamEvent>,
    ) {
        panic!("recorder must not be used")
    }
    fn origin(&self, _: mio_core::effect::EffectId, _: &mio_core::message::Origin) {
        panic!("recorder must not be used")
    }
    fn tool_output(&self, _: mio_core::effect::EffectId, _: mio_core::tool::ToolResultContext) {
        panic!("recorder must not be used")
    }
    fn resolve(
        &self,
        _: mio_core::effect::EffectId,
        _: Result<mio_core::effect::Outcome, mio_core::error::ErrorReport>,
    ) {
        panic!("recorder must not be used")
    }
}
