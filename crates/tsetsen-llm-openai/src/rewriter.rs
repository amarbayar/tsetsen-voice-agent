use async_trait::async_trait;
use tsetsen_core::ports::{Rewrite, RewriteRequest, Rewriter};
use tsetsen_core::{Ctx, Describe, Layer, ProviderDescriptor, ProviderError};

use crate::client::ChatClient;
use crate::config::{ConfigError, OpenAiConfig};
use crate::prompt;

/// Transcript cleanup through any OpenAI-compatible chat endpoint.
pub struct OpenAiRewriter {
    descriptor: ProviderDescriptor,
    client: ChatClient,
}

impl OpenAiRewriter {
    /// Builds a rewriter for the endpoint in `config`.
    pub fn new(config: &OpenAiConfig) -> Result<Self, ConfigError> {
        let mut descriptor =
            ProviderDescriptor::new(config.id.clone(), Layer::Rewrite, config.locality.clone());
        if let Some(languages) = &config.languages {
            descriptor = descriptor.with_languages(languages.iter().cloned());
        }
        Ok(Self {
            descriptor,
            client: ChatClient::new(config)?,
        })
    }
}

impl Describe for OpenAiRewriter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl Rewriter for OpenAiRewriter {
    async fn rewrite(&self, request: &RewriteRequest, cx: &Ctx) -> Result<Rewrite, ProviderError> {
        let output = self
            .client
            .complete(
                &prompt::messages(request),
                prompt::max_tokens(&request.transcript),
                cx,
            )
            .await?;
        Ok(Rewrite {
            text: prompt::extract(&output),
        })
    }
}
