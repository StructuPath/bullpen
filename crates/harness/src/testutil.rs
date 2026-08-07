//! Shared test doubles for harness tests.

use bullpen_llm::{ContentBlock, Provider, ProviderError, Request, Response, StopReason, Usage};
use std::sync::{Arc, Mutex};

pub struct FakeProvider {
    pub script: Mutex<Vec<Response>>,
}

impl FakeProvider {
    pub fn new(mut responses: Vec<Response>) -> Arc<Self> {
        responses.reverse();
        Arc::new(Self {
            script: Mutex::new(responses),
        })
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }
    async fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
        self.script
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| ProviderError::Malformed("script exhausted".into()))
    }
}

pub fn response(blocks: Vec<ContentBlock>, stop: StopReason) -> Response {
    Response {
        content: blocks,
        stop_reason: stop,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

pub fn text_response(text: &str) -> Response {
    response(
        vec![ContentBlock::Text { text: text.into() }],
        StopReason::EndTurn,
    )
}
