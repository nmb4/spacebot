//! SpacebotModel: Custom CompletionModel implementation that routes through LlmManager.

use crate::config::{ApiType, ProviderConfig};
use crate::llm::manager::LlmManager;
use crate::llm::routing::{
    self, MAX_FALLBACK_ATTEMPTS, MAX_RETRIES_PER_MODEL, RETRY_BASE_DELAY_MS, RoutingConfig,
};

use rig::completion::{self, CompletionError, CompletionModel, CompletionRequest, GetTokenUsage};
use rig::message::{
    AssistantContent, DocumentSourceKind, Image, Message, MimeType, Text, ToolCall, ToolFunction,
    UserContent,
};
use rig::one_or_many::OneOrMany;
use rig::streaming::StreamingCompletionResponse;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Raw provider response. Wraps the JSON so Rig can carry it through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawResponse {
    pub body: serde_json::Value,
}

/// Streaming response placeholder. Streaming will be implemented per-provider
/// when we wire up SSE parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawStreamingResponse {
    pub body: serde_json::Value,
}

impl GetTokenUsage for RawStreamingResponse {
    fn token_usage(&self) -> Option<completion::Usage> {
        None
    }
}

/// Custom completion model that routes through LlmManager.
///
/// Optionally holds a RoutingConfig for fallback behavior. When present,
/// completion() will try fallback models on retriable errors.
#[derive(Clone)]
pub struct SpacebotModel {
    llm_manager: Arc<LlmManager>,
    model_name: String,
    provider: String,
    full_model_name: String,
    routing: Option<RoutingConfig>,
    agent_id: Option<String>,
    process_type: Option<String>,
}

impl SpacebotModel {
    pub fn provider(&self) -> &str {
        &self.provider
    }
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
    pub fn full_model_name(&self) -> &str {
        &self.full_model_name
    }

    /// Attach routing config for fallback behavior.
    pub fn with_routing(mut self, routing: RoutingConfig) -> Self {
        self.routing = Some(routing);
        self
    }

    /// Attach agent context for per-agent metric labels.
    pub fn with_context(
        mut self,
        agent_id: impl Into<String>,
        process_type: impl Into<String>,
    ) -> Self {
        self.agent_id = Some(agent_id.into());
        self.process_type = Some(process_type.into());
        self
    }

    /// Direct call to the provider (no fallback logic).
    async fn attempt_completion(
        &self,
        request: CompletionRequest,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        let provider_id = self
            .full_model_name
            .split_once('/')
            .map(|(provider, _)| provider)
            .unwrap_or("anthropic");

        let provider_config = match provider_id {
            "anthropic" => self
                .llm_manager
                .get_anthropic_provider()
                .await
                .map_err(|e| CompletionError::ProviderError(e.to_string()))?,
            "antigravity" => self
                .llm_manager
                .get_antigravity_provider()
                .await
                .map_err(|e| CompletionError::ProviderError(e.to_string()))?,
            _ => self
                .llm_manager
                .get_provider(provider_id)
                .map_err(|e| CompletionError::ProviderError(e.to_string()))?,
        };

        if provider_id == "zai-coding-plan" || provider_id == "zhipu" {
            let display_name = if provider_id == "zhipu" {
                "Z.AI (GLM)"
            } else {
                "Z.AI Coding Plan"
            };
            let endpoint = format!(
                "{}/chat/completions",
                provider_config.base_url.trim_end_matches('/')
            );
            return self
                .call_openai_compatible_with_optional_auth(
                    request,
                    display_name,
                    &endpoint,
                    Some(provider_config.api_key.clone()),
                )
                .await;
        }

        match provider_config.api_type {
            ApiType::Anthropic => self.call_anthropic(request, &provider_config).await,
            ApiType::OpenAiCompletions => self.call_openai(request, &provider_config).await,
            ApiType::OpenAiResponses => self.call_openai_responses(request, &provider_config).await,
            ApiType::Gemini => {
                self.call_openai_compatible(request, "Google Gemini", &provider_config)
                    .await
            }
            ApiType::Antigravity => self.call_antigravity(request, &provider_config).await,
        }
    }

    /// Try a model with retries and exponential backoff on transient errors.
    ///
    /// Returns `Ok(response)` on success, or `Err((last_error, was_rate_limit))`
    /// after exhausting retries. `was_rate_limit` indicates the final failure was
    /// a 429/rate-limit (as opposed to a timeout or server error), so the caller
    /// can decide whether to record cooldown.
    async fn attempt_with_retries(
        &self,
        model_name: &str,
        request: &CompletionRequest,
    ) -> Result<completion::CompletionResponse<RawResponse>, (CompletionError, bool)> {
        let model = if model_name == self.full_model_name {
            self.clone()
        } else {
            SpacebotModel::make(&self.llm_manager, model_name)
        };

        let mut last_error = None;
        for attempt in 0..MAX_RETRIES_PER_MODEL {
            if attempt > 0 {
                let delay_ms = RETRY_BASE_DELAY_MS * 2u64.pow((attempt - 1) as u32);
                tracing::debug!(
                    model = %model_name,
                    attempt = attempt + 1,
                    delay_ms,
                    "retrying after backoff"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            match model.attempt_completion(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let error_str = error.to_string();
                    if !routing::is_retriable_error(&error_str) {
                        // Non-retriable (auth error, bad request, etc) — bail immediately
                        return Err((error, false));
                    }
                    tracing::warn!(
                        model = %model_name,
                        attempt = attempt + 1,
                        %error,
                        "retriable error"
                    );
                    last_error = Some(error_str);
                }
            }
        }

        let error_str = last_error.unwrap_or_default();
        let was_rate_limit = routing::is_rate_limit_error(&error_str);
        Err((
            CompletionError::ProviderError(format!(
                "{model_name} failed after {MAX_RETRIES_PER_MODEL} attempts: {error_str}"
            )),
            was_rate_limit,
        ))
    }
}

impl CompletionModel for SpacebotModel {
    type Response = RawResponse;
    type StreamingResponse = RawStreamingResponse;
    type Client = Arc<LlmManager>;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        let full_name = model.into();

        // OpenRouter model names have the form "openrouter/provider/model",
        // so split on the first "/" only and keep the rest as the model name.
        let (provider, model_name) = if let Some(rest) = full_name.strip_prefix("openrouter/") {
            ("openrouter".to_string(), rest.to_string())
        } else if let Some((p, m)) = full_name.split_once('/') {
            (p.to_string(), m.to_string())
        } else {
            ("anthropic".to_string(), full_name.clone())
        };

        let full_model_name = format!("{provider}/{model_name}");

        Self {
            llm_manager: client.clone(),
            model_name,
            provider,
            full_model_name,
            routing: None,
            agent_id: None,
            process_type: None,
        }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        #[cfg(feature = "metrics")]
        let start = std::time::Instant::now();

        let result = async move {
            let Some(routing) = &self.routing else {
                // No routing config — just call the model directly, no fallback/retry
                return self.attempt_completion(request).await;
            };

            let cooldown = routing.rate_limit_cooldown_secs;
            let fallbacks = routing.get_fallbacks(&self.full_model_name);
            let mut last_error: Option<CompletionError> = None;

            // Try the primary model (with retries) unless it's in rate-limit cooldown
            // and we have fallbacks to try instead.
            let primary_rate_limited = self
                .llm_manager
                .is_rate_limited(&self.full_model_name, cooldown)
                .await;

            let skip_primary = primary_rate_limited && !fallbacks.is_empty();

            if skip_primary {
                tracing::debug!(
                    model = %self.full_model_name,
                    "primary model in rate-limit cooldown, skipping to fallbacks"
                );
            } else {
                match self
                    .attempt_with_retries(&self.full_model_name, &request)
                    .await
                {
                    Ok(response) => return Ok(response),
                    Err((error, was_rate_limit)) => {
                        if was_rate_limit {
                            self.llm_manager
                                .record_rate_limit(&self.full_model_name)
                                .await;
                        }
                        if fallbacks.is_empty() {
                            // No fallbacks — this is the final error
                            return Err(error);
                        }
                        tracing::warn!(
                            model = %self.full_model_name,
                            "primary model exhausted retries, trying fallbacks"
                        );
                        last_error = Some(error);
                    }
                }
            }

            // Try fallback chain, each with their own retry loop
            for (index, fallback_name) in fallbacks.iter().take(MAX_FALLBACK_ATTEMPTS).enumerate() {
                if self
                    .llm_manager
                    .is_rate_limited(fallback_name, cooldown)
                    .await
                {
                    tracing::debug!(
                        fallback = %fallback_name,
                        "fallback model in cooldown, skipping"
                    );
                    continue;
                }

                match self.attempt_with_retries(fallback_name, &request).await {
                    Ok(response) => {
                        tracing::info!(
                            original = %self.full_model_name,
                            fallback = %fallback_name,
                            attempt = index + 1,
                            "fallback model succeeded"
                        );
                        return Ok(response);
                    }
                    Err((error, was_rate_limit)) => {
                        if was_rate_limit {
                            self.llm_manager.record_rate_limit(fallback_name).await;
                        }
                        tracing::warn!(
                            fallback = %fallback_name,
                            "fallback model exhausted retries, continuing chain"
                        );
                        last_error = Some(error);
                    }
                }
            }

            Err(last_error.unwrap_or_else(|| {
                CompletionError::ProviderError("all models in fallback chain failed".into())
            }))
        }
        .await;

        #[cfg(feature = "metrics")]
        {
            let elapsed = start.elapsed().as_secs_f64();
            let agent_label = self.agent_id.as_deref().unwrap_or("unknown");
            let tier_label = self.process_type.as_deref().unwrap_or("unknown");
            let metrics = crate::telemetry::Metrics::global();
            metrics
                .llm_requests_total
                .with_label_values(&[agent_label, &self.full_model_name, tier_label])
                .inc();
            metrics
                .llm_request_duration_seconds
                .with_label_values(&[agent_label, &self.full_model_name, tier_label])
                .observe(elapsed);

            if let Ok(ref response) = result {
                let usage = &response.usage;
                if usage.input_tokens > 0 || usage.output_tokens > 0 {
                    metrics
                        .llm_tokens_total
                        .with_label_values(&[
                            agent_label,
                            &self.full_model_name,
                            tier_label,
                            "input",
                        ])
                        .inc_by(usage.input_tokens);
                    metrics
                        .llm_tokens_total
                        .with_label_values(&[
                            agent_label,
                            &self.full_model_name,
                            tier_label,
                            "output",
                        ])
                        .inc_by(usage.output_tokens);
                    if usage.cached_input_tokens > 0 {
                        metrics
                            .llm_tokens_total
                            .with_label_values(&[
                                agent_label,
                                &self.full_model_name,
                                tier_label,
                                "cached_input",
                            ])
                            .inc_by(usage.cached_input_tokens);
                    }

                    let cost = crate::llm::pricing::estimate_cost(
                        &self.full_model_name,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cached_input_tokens,
                    );
                    if cost > 0.0 {
                        metrics
                            .llm_estimated_cost_dollars
                            .with_label_values(&[agent_label, &self.full_model_name, tier_label])
                            .inc_by(cost);
                    }
                }
            }

            if let Err(ref error) = result {
                let error_type = match error {
                    rig::completion::CompletionError::ProviderError(msg) => {
                        if msg.contains("rate") || msg.contains("429") {
                            "rate_limit"
                        } else if msg.contains("timeout") {
                            "timeout"
                        } else if msg.contains("context") || msg.contains("too long") {
                            "context_overflow"
                        } else {
                            "provider_error"
                        }
                    }
                    _ => "other",
                };
                metrics
                    .process_errors_total
                    .with_label_values(&[agent_label, tier_label, error_type])
                    .inc();
            }
        }

        result
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<RawStreamingResponse>, CompletionError> {
        Err(CompletionError::ProviderError(
            "streaming not yet implemented".into(),
        ))
    }
}

impl SpacebotModel {
    async fn call_anthropic(
        &self,
        request: CompletionRequest,
        provider_config: &ProviderConfig,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        let api_key = provider_config.api_key.as_str();

        let effort = self
            .routing
            .as_ref()
            .map(|r| r.thinking_effort_for_model(&self.model_name))
            .unwrap_or("auto");
        let anthropic_request = crate::llm::anthropic::build_anthropic_request(
            self.llm_manager.http_client(),
            api_key,
            &provider_config.base_url,
            &self.model_name,
            &request,
            effort,
        );

        let is_oauth =
            anthropic_request.auth_path == crate::llm::anthropic::AnthropicAuthPath::OAuthToken;
        let original_tools = anthropic_request.original_tools;

        let response = anthropic_request
            .builder
            .send()
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;

        let status = response.status();
        let response_text = response.text().await.map_err(|e| {
            CompletionError::ProviderError(format!("failed to read response body: {e}"))
        })?;

        let response_body: serde_json::Value =
            serde_json::from_str(&response_text).map_err(|e| {
                CompletionError::ProviderError(format!(
                    "Anthropic response ({status}) is not valid JSON: {e}\nBody: {}",
                    truncate_body(&response_text)
                ))
            })?;

        if !status.is_success() {
            let message = response_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(CompletionError::ProviderError(format!(
                "Anthropic API error ({status}): {message}"
            )));
        }

        let mut completion = parse_anthropic_response(response_body)?;

        // Reverse-map tool names when using OAuth (Claude Code canonical → original)
        if is_oauth && !original_tools.is_empty() {
            reverse_map_tool_names(&mut completion, &original_tools);
        }

        Ok(completion)
    }

    async fn call_openai(
        &self,
        request: CompletionRequest,
        provider_config: &ProviderConfig,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        let api_key = provider_config.api_key.as_str();

        let include_reasoning_content = provider_config.base_url.contains("kimi.com");
        let mut messages = Vec::new();

        if let Some(preamble) = &request.preamble {
            messages.push(serde_json::json!({
                "role": "system",
                "content": preamble,
            }));
        }

        messages.extend(convert_messages_to_openai(
            &request.chat_history,
            include_reasoning_content,
        ));

        let mut body = serde_json::json!({
            "model": self.model_name,
            "messages": messages,
        });

        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }

        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }

        let chat_completions_url = format!(
            "{}/v1/chat/completions",
            provider_config.base_url.trim_end_matches('/')
        );

        let mut request_builder = self
            .llm_manager
            .http_client()
            .post(&chat_completions_url)
            .header("content-type", "application/json");

        if !api_key.is_empty() {
            request_builder = request_builder.header("authorization", format!("Bearer {api_key}"));
        }

        // Kimi endpoints require a specific user-agent header.
        if chat_completions_url.contains("kimi.com") || chat_completions_url.contains("moonshot.ai")
        {
            request_builder = request_builder.header("user-agent", "KimiCLI/1.3");
        }

        let response = request_builder
            .json(&body)
            .send()
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;

        let status = response.status();
        let response_text = response.text().await.map_err(|e| {
            CompletionError::ProviderError(format!("failed to read response body: {e}"))
        })?;

        let response_body: serde_json::Value =
            serde_json::from_str(&response_text).map_err(|e| {
                CompletionError::ProviderError(format!(
                    "OpenAI response ({status}) is not valid JSON: {e}\nBody: {}",
                    truncate_body(&response_text)
                ))
            })?;

        if !status.is_success() {
            let message = response_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(CompletionError::ProviderError(format!(
                "OpenAI API error ({status}): {message}"
            )));
        }

        parse_openai_response(response_body, "OpenAI")
    }

    async fn call_openai_responses(
        &self,
        request: CompletionRequest,
        provider_config: &ProviderConfig,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        let base_url = provider_config.base_url.trim_end_matches('/');
        let responses_url = format!("{base_url}/v1/responses");
        let api_key = provider_config.api_key.as_str();

        let input = convert_messages_to_openai_responses(&request.chat_history);

        let mut body = serde_json::json!({
            "model": self.model_name,
            "input": input,
        });

        if let Some(preamble) = &request.preamble {
            body["instructions"] = serde_json::json!(preamble);
        }

        if let Some(max_tokens) = request.max_tokens {
            body["max_output_tokens"] = serde_json::json!(max_tokens);
        }

        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }

        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|tool_definition| {
                    serde_json::json!({
                        "type": "function",
                        "name": tool_definition.name,
                        "description": tool_definition.description,
                        "parameters": tool_definition.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }

        let mut request_builder = self
            .llm_manager
            .http_client()
            .post(&responses_url)
            .header("content-type", "application/json");
        if !api_key.is_empty() {
            request_builder = request_builder.header("authorization", format!("Bearer {api_key}"));
        }

        let response = request_builder
            .json(&body)
            .send()
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;

        let status = response.status();
        let response_text = response.text().await.map_err(|e| {
            CompletionError::ProviderError(format!("failed to read response body: {e}"))
        })?;

        let response_body: serde_json::Value =
            serde_json::from_str(&response_text).map_err(|e| {
                CompletionError::ProviderError(format!(
                    "OpenAI Responses API response ({status}) is not valid JSON: {e}\nBody: {}",
                    truncate_body(&response_text)
                ))
            })?;

        if !status.is_success() {
            let message = response_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(CompletionError::ProviderError(format!(
                "OpenAI Responses API error ({status}): {message}"
            )));
        }

        parse_openai_responses_response(response_body)
    }

    /// Generic OpenAI-compatible API call.
    /// Used by providers that implement the OpenAI chat completions format.
    #[allow(dead_code)]
    async fn call_openai_compatible(
        &self,
        request: CompletionRequest,
        provider_display_name: &str,
        provider_config: &ProviderConfig,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        let base_url = provider_config.base_url.trim_end_matches('/');
        let endpoint_path = match provider_config.api_type {
            ApiType::OpenAiCompletions | ApiType::OpenAiResponses => "/v1/chat/completions",
            ApiType::Gemini => "/chat/completions",
            ApiType::Anthropic | ApiType::Antigravity => {
                return Err(CompletionError::ProviderError(format!(
                    "{provider_display_name} is configured with a non-OpenAI API type, but this call expects an OpenAI-compatible API"
                )));
            }
        };
        let endpoint = format!("{base_url}{endpoint_path}");
        let api_key = provider_config.api_key.as_str();

        let mut messages = Vec::new();

        if let Some(preamble) = &request.preamble {
            messages.push(serde_json::json!({
                "role": "system",
                "content": preamble,
            }));
        }

        messages.extend(convert_messages_to_openai(&request.chat_history, false));

        let mut body = serde_json::json!({
            "model": self.model_name,
            "messages": messages,
        });

        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }

        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }

        let response = self
            .llm_manager
            .http_client()
            .post(&endpoint)
            .header("authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;

        let status = response.status();
        let response_text = response.text().await.map_err(|e| {
            CompletionError::ProviderError(format!("failed to read response body: {e}"))
        })?;

        let response_body: serde_json::Value =
            serde_json::from_str(&response_text).map_err(|e| {
                CompletionError::ProviderError(format!(
                    "{provider_display_name} response ({status}) is not valid JSON: {e}\nBody: {}",
                    truncate_body(&response_text)
                ))
            })?;

        if !status.is_success() {
            let message = response_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(CompletionError::ProviderError(format!(
                "{provider_display_name} API error ({status}): {message}"
            )));
        }

        parse_openai_response(response_body, provider_display_name)
    }

    /// Generic OpenAI-compatible API call with optional bearer auth.
    async fn call_openai_compatible_with_optional_auth(
        &self,
        request: CompletionRequest,
        provider_display_name: &str,
        endpoint: &str,
        api_key: Option<String>,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        let mut messages = Vec::new();

        if let Some(preamble) = &request.preamble {
            messages.push(serde_json::json!({
                "role": "system",
                "content": preamble,
            }));
        }

        messages.extend(convert_messages_to_openai(&request.chat_history, false));

        let mut body = serde_json::json!({
            "model": self.model_name,
            "messages": messages,
        });

        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }

        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }

        let response = self.llm_manager.http_client().post(endpoint);

        let response = if let Some(api_key) = api_key {
            response.header("authorization", format!("Bearer {api_key}"))
        } else {
            response
        };

        let response = response
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;

        let status = response.status();
        let response_text = response.text().await.map_err(|e| {
            CompletionError::ProviderError(format!("failed to read response body: {e}"))
        })?;

        let response_body: serde_json::Value =
            serde_json::from_str(&response_text).map_err(|e| {
                CompletionError::ProviderError(format!(
                    "{provider_display_name} response ({status}) is not valid JSON: {e}\nBody: {}",
                    truncate_body(&response_text)
                ))
            })?;

        if !status.is_success() {
            let message = response_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(CompletionError::ProviderError(format!(
                "{provider_display_name} API error ({status}): {message}"
            )));
        }

        parse_openai_response(response_body, provider_display_name)
    }

    async fn call_antigravity(
        &self,
        request: CompletionRequest,
        provider_config: &ProviderConfig,
    ) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
        let project_id = provider_config.name.clone().ok_or_else(|| {
            CompletionError::ProviderError(
                "antigravity provider requires a project_id (set llm.antigravity_project_id or use antigravity OAuth login)"
                    .to_string(),
            )
        })?;
        let project_id = project_id
            .trim()
            .trim_start_matches("projects/")
            .to_string();
        if project_id.is_empty() {
            return Err(CompletionError::ProviderError(
                "antigravity project_id is empty; re-run `spacebot auth login --provider antigravity`"
                    .to_string(),
            ));
        }

        let antigravity_version =
            std::env::var("PI_AI_ANTIGRAVITY_VERSION").unwrap_or_else(|_| "1.15.8".to_string());
        let model_candidates = antigravity_model_candidates(&self.model_name);

        let mut endpoints = vec!["https://daily-cloudcode-pa.sandbox.googleapis.com".to_string()];
        let configured = provider_config.base_url.trim_end_matches('/').to_string();
        if !configured.is_empty() && !endpoints.contains(&configured) {
            endpoints.push(configured);
        }
        let default_endpoint = "https://cloudcode-pa.googleapis.com".to_string();
        if !endpoints.contains(&default_endpoint) {
            endpoints.push(default_endpoint);
        }

        let mut last_error = None;
        for model_name in &model_candidates {
            if model_name != &self.model_name {
                tracing::info!(
                    requested_model = %self.model_name,
                    fallback_model = %model_name,
                    "trying Antigravity model fallback"
                );
            }

            let body = build_antigravity_request(&project_id, model_name, &request);
            let is_claude_thinking = antigravity_requires_claude_thinking_header(model_name);

            for base_endpoint in &endpoints {
                let endpoint = format!("{base_endpoint}/v1internal:streamGenerateContent?alt=sse");

                let mut request_builder = self
                    .llm_manager
                    .http_client()
                    .post(&endpoint)
                    .header("authorization", format!("Bearer {}", provider_config.api_key))
                    .header("accept", "text/event-stream")
                    .header("content-type", "application/json")
                    .header(
                        "user-agent",
                        format!("antigravity/{antigravity_version} darwin/arm64"),
                    )
                    .header(
                        "x-goog-api-client",
                        "google-cloud-sdk vscode_cloudshelleditor/0.1",
                    )
                    .header(
                        "client-metadata",
                        r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
                    )
                    .json(&body);

                if is_claude_thinking {
                    request_builder =
                        request_builder.header("anthropic-beta", "interleaved-thinking-2025-05-14");
                }

                let response = request_builder
                    .send()
                    .await
                    .map_err(|e| CompletionError::ProviderError(e.to_string()))?;

                let status = response.status();
                let response_text = response.text().await.map_err(|e| {
                    CompletionError::ProviderError(format!("failed to read response body: {e}"))
                })?;

                if !status.is_success() {
                    let message = serde_json::from_str::<serde_json::Value>(&response_text)
                        .ok()
                        .and_then(|json| json["error"]["message"].as_str().map(ToOwned::to_owned))
                        .unwrap_or_else(|| truncate_body(&response_text).to_string());
                    let error = CompletionError::ProviderError(format!(
                        "Antigravity API error ({status}) from {base_endpoint} using model {model_name}: {message}"
                    ));

                    if status == reqwest::StatusCode::NOT_FOUND {
                        last_error = Some(error);
                        continue;
                    }

                    if should_try_next_antigravity_model(status, &message) {
                        last_error = Some(error);
                        break;
                    }

                    return Err(error);
                }

                let events = parse_sse_events(&response_text);
                if events.is_empty() {
                    return Err(CompletionError::ResponseError(
                        "empty SSE response from Antigravity".into(),
                    ));
                }

                return parse_antigravity_response(events);
            }
        }

        Err(last_error.unwrap_or_else(|| {
            CompletionError::ProviderError("Antigravity API request failed".to_string())
        }))
    }
}
// --- Helpers ---

/// Reverse-map Claude Code canonical tool names back to the original names
/// from the request's tool definitions.
fn reverse_map_tool_names(
    completion: &mut completion::CompletionResponse<RawResponse>,
    original_tools: &[(String, String)],
) {
    for content in completion.choice.iter_mut() {
        if let AssistantContent::ToolCall(tc) = content {
            tc.function.name =
                crate::llm::anthropic::from_claude_code_name(&tc.function.name, original_tools);
        }
    }
}

fn tool_result_content_to_string(content: &OneOrMany<rig::message::ToolResultContent>) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            rig::message::ToolResultContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// --- Message conversion ---

pub fn convert_messages_to_anthropic(messages: &OneOrMany<Message>) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|message| match message {
            Message::User { content } => {
                let parts: Vec<serde_json::Value> = content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => {
                            Some(serde_json::json!({"type": "text", "text": t.text}))
                        }
                        UserContent::Image(image) => convert_image_anthropic(image),
                        UserContent::ToolResult(result) => Some(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": result.id,
                            "content": tool_result_content_to_string(&result.content),
                        })),
                        _ => None,
                    })
                    .collect();
                serde_json::json!({"role": "user", "content": parts})
            }
            Message::Assistant { content, .. } => {
                let parts: Vec<serde_json::Value> = content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(t) => {
                            Some(serde_json::json!({"type": "text", "text": t.text}))
                        }
                        AssistantContent::ToolCall(tc) => Some(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": tc.function.arguments,
                        })),
                        _ => None,
                    })
                    .collect();
                serde_json::json!({"role": "assistant", "content": parts})
            }
        })
        .collect()
}

fn convert_messages_to_openai(
    messages: &OneOrMany<Message>,
    include_reasoning_content: bool,
) -> Vec<serde_json::Value> {
    let mut result = Vec::new();

    for message in messages.iter() {
        match message {
            Message::User { content } => {
                // Separate tool results (they need their own messages) from content parts
                let mut content_parts: Vec<serde_json::Value> = Vec::new();
                let mut tool_results: Vec<serde_json::Value> = Vec::new();

                for item in content.iter() {
                    match item {
                        UserContent::Text(t) => {
                            content_parts.push(serde_json::json!({
                                "type": "text",
                                "text": t.text,
                            }));
                        }
                        UserContent::Image(image) => {
                            if let Some(part) = convert_image_openai(image) {
                                content_parts.push(part);
                            }
                        }
                        UserContent::ToolResult(tr) => {
                            tool_results.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tr.id,
                                "content": tool_result_content_to_string(&tr.content),
                            }));
                        }
                        _ => {}
                    }
                }

                if !content_parts.is_empty() {
                    // If there's only one text part and no images, use simple string format
                    if content_parts.len() == 1 && content_parts[0]["type"] == "text" {
                        result.push(serde_json::json!({
                            "role": "user",
                            "content": content_parts[0]["text"],
                        }));
                    } else {
                        // Mixed content (text + images): use array-of-parts format
                        result.push(serde_json::json!({
                            "role": "user",
                            "content": content_parts,
                        }));
                    }
                }

                result.extend(tool_results);
            }
            Message::Assistant { content, .. } => {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();
                let mut reasoning_parts = Vec::new();

                for item in content.iter() {
                    match item {
                        AssistantContent::Text(t) => {
                            text_parts.push(t.text.clone());
                        }
                        AssistantContent::ToolCall(tc) => {
                            // OpenAI expects arguments as a JSON string
                            let args_string = serde_json::to_string(&tc.function.arguments)
                                .unwrap_or_else(|_| "{}".to_string());
                            tool_calls.push(serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.function.name,
                                    "arguments": args_string,
                                }
                            }));
                        }
                        AssistantContent::Reasoning(reasoning) => {
                            reasoning_parts.extend(reasoning.reasoning.iter().cloned());
                        }
                        _ => {}
                    }
                }

                let mut msg = serde_json::json!({"role": "assistant"});
                if !text_parts.is_empty() {
                    msg["content"] = serde_json::json!(text_parts.join("\n"));
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = serde_json::json!(tool_calls);
                }
                if include_reasoning_content && !tool_calls.is_empty() {
                    msg["reasoning_content"] = serde_json::json!(reasoning_parts.join("\n"));
                }
                result.push(msg);
            }
        }
    }

    result
}

fn convert_messages_to_openai_responses(messages: &OneOrMany<Message>) -> Vec<serde_json::Value> {
    let mut result = Vec::new();

    for message in messages.iter() {
        match message {
            Message::User { content } => {
                let mut content_parts = Vec::new();

                for item in content.iter() {
                    match item {
                        UserContent::Text(text) => {
                            content_parts.push(serde_json::json!({
                                "type": "input_text",
                                "text": text.text,
                            }));
                        }
                        UserContent::Image(image) => {
                            if let Some(part) = convert_image_openai_responses(image) {
                                content_parts.push(part);
                            }
                        }
                        UserContent::ToolResult(tool_result) => {
                            result.push(serde_json::json!({
                                "type": "function_call_output",
                                "call_id": tool_result.id,
                                "output": tool_result_content_to_string(&tool_result.content),
                            }));
                        }
                        _ => {}
                    }
                }

                if !content_parts.is_empty() {
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": content_parts,
                    }));
                }
            }
            Message::Assistant { content, .. } => {
                let mut text_parts = Vec::new();

                for item in content.iter() {
                    match item {
                        AssistantContent::Text(text) => {
                            text_parts.push(serde_json::json!({
                                "type": "output_text",
                                "text": text.text,
                            }));
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let arguments = serde_json::to_string(&tool_call.function.arguments)
                                .unwrap_or_else(|_| "{}".to_string());
                            result.push(serde_json::json!({
                                "type": "function_call",
                                "name": tool_call.function.name,
                                "arguments": arguments,
                                "call_id": tool_call.id,
                            }));
                        }
                        _ => {}
                    }
                }

                if !text_parts.is_empty() {
                    result.push(serde_json::json!({
                        "role": "assistant",
                        "content": text_parts,
                    }));
                }
            }
        }
    }

    result
}

const ANTIGRAVITY_SYSTEM_INSTRUCTION: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding. You are pair programming with a USER to solve their coding task.";

fn antigravity_requires_claude_thinking_header(model_name: &str) -> bool {
    model_name.starts_with("claude-")
        && (model_name.contains("thinking") || model_name == "claude-sonnet-4-6")
}

fn antigravity_model_candidates(requested_model_name: &str) -> Vec<String> {
    let mut model_candidates = Vec::new();
    let mut add_model = |model_name: &str| {
        if !model_candidates
            .iter()
            .any(|existing| existing == model_name)
        {
            model_candidates.push(model_name.to_string());
        }
    };

    let prefer_alias_first = matches!(
        requested_model_name,
        "claude-opus-4-5"
            | "claude-opus-4-5-thinking"
            | "claude-sonnet-4-5"
            | "claude-sonnet-4-5-thinking"
    );

    if prefer_alias_first {
        if let Some(alias) = antigravity_model_alias(requested_model_name) {
            add_model(alias);
        }
        add_model(requested_model_name);
    } else {
        add_model(requested_model_name);
        if let Some(alias) = antigravity_model_alias(requested_model_name) {
            add_model(alias);
        }
    }

    if requested_model_name.starts_with("claude-sonnet-4-")
        || requested_model_name == "claude-sonnet-4-6-thinking"
    {
        add_model("claude-sonnet-4-6");
        add_model("claude-sonnet-4-5-thinking");
        add_model("claude-sonnet-4-5");
    }

    if requested_model_name.starts_with("claude-opus-4-") {
        add_model("claude-opus-4-6-thinking");
        add_model("claude-opus-4-5-thinking");
    }

    if requested_model_name.starts_with("gemini-3-pro")
        || requested_model_name.starts_with("gemini-3.1-pro")
    {
        add_model("gemini-3.1-pro-high");
        add_model("gemini-3.1-pro-low");
        add_model("gemini-3-pro-high");
        add_model("gemini-3-pro-low");
    }

    if requested_model_name == "gemini-3-pro" || requested_model_name == "gemini-3.1-pro" {
        add_model("gemini-3.1-pro-high");
        add_model("gemini-3-pro-high");
    }

    model_candidates
}

fn antigravity_model_alias(model_name: &str) -> Option<&'static str> {
    match model_name {
        "claude-opus-4-5" => Some("claude-opus-4-6-thinking"),
        "claude-opus-4-5-thinking" => Some("claude-opus-4-6-thinking"),
        "claude-opus-4-6" => Some("claude-opus-4-6-thinking"),
        "claude-sonnet-4-5" => Some("claude-sonnet-4-6"),
        "claude-sonnet-4-5-thinking" => Some("claude-sonnet-4-6"),
        "claude-sonnet-4-6-thinking" => Some("claude-sonnet-4-6"),
        "gemini-3-pro" => Some("gemini-3-pro-high"),
        "gemini-3.1-pro" => Some("gemini-3.1-pro-high"),
        _ => None,
    }
}

fn should_try_next_antigravity_model(status: reqwest::StatusCode, message: &str) -> bool {
    let message_lower = message.to_ascii_lowercase();
    let mentions_model = message_lower.contains("model");
    let looks_like_missing_or_mismatch = message_lower.contains("not found")
        || message_lower.contains("requested entity")
        || message_lower.contains("unknown")
        || message_lower.contains("unsupported")
        || message_lower.contains("unavailable")
        || message_lower.contains("no longer available");

    status == reqwest::StatusCode::NOT_FOUND
        || (status == reqwest::StatusCode::BAD_REQUEST && looks_like_missing_or_mismatch)
        || (status == reqwest::StatusCode::FORBIDDEN
            && mentions_model
            && looks_like_missing_or_mismatch)
}

fn build_antigravity_request(
    project_id: &str,
    model_name: &str,
    request: &CompletionRequest,
) -> serde_json::Value {
    let mut inner_request = serde_json::json!({
        "contents": convert_messages_to_antigravity_gemini(&request.chat_history),
    });

    if let Some(preamble) = &request.preamble {
        let mut parts = vec![
            serde_json::json!({ "text": ANTIGRAVITY_SYSTEM_INSTRUCTION }),
            serde_json::json!({ "text": format!("Please ignore following [ignore]{}[/ignore]", ANTIGRAVITY_SYSTEM_INSTRUCTION) }),
        ];
        parts.push(serde_json::json!({ "text": preamble }));
        inner_request["systemInstruction"] = serde_json::json!({
            "role": "user",
            "parts": parts,
        });
    } else {
        inner_request["systemInstruction"] = serde_json::json!({
            "role": "user",
            "parts": [
                { "text": ANTIGRAVITY_SYSTEM_INSTRUCTION },
                { "text": format!("Please ignore following [ignore]{}[/ignore]", ANTIGRAVITY_SYSTEM_INSTRUCTION) }
            ],
        });
    }

    let mut generation_config = serde_json::Map::new();
    if let Some(max_tokens) = request.max_tokens {
        generation_config.insert("maxOutputTokens".to_string(), serde_json::json!(max_tokens));
    }
    if let Some(temperature) = request.temperature {
        generation_config.insert("temperature".to_string(), serde_json::json!(temperature));
    }
    if !generation_config.is_empty() {
        inner_request["generationConfig"] = serde_json::Value::Object(generation_config);
    }

    if !request.tools.is_empty() {
        let declarations: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut declaration = serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                });
                declaration["parameters"] = tool.parameters.clone();
                declaration
            })
            .collect();
        inner_request["tools"] = serde_json::json!([{
            "functionDeclarations": declarations,
        }]);
    }

    let request_id = format!(
        "agent-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        uuid::Uuid::new_v4().simple()
    );

    serde_json::json!({
        "project": project_id,
        "model": model_name,
        "request": inner_request,
        "requestType": "agent",
        "userAgent": "antigravity",
        "requestId": request_id,
    })
}

fn convert_messages_to_antigravity_gemini(messages: &OneOrMany<Message>) -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    let mut tool_call_name_by_id: HashMap<String, String> = HashMap::new();

    for message in messages.iter() {
        match message {
            Message::User { content } => {
                let mut parts = Vec::new();

                for item in content.iter() {
                    match item {
                        UserContent::Text(text) => {
                            parts.push(serde_json::json!({ "text": text.text }));
                        }
                        UserContent::ToolResult(tool_result) => {
                            let tool_name = tool_call_name_by_id
                                .get(&tool_result.id)
                                .cloned()
                                .unwrap_or_else(|| "tool_result".to_string());
                            parts.push(serde_json::json!({
                                "functionResponse": {
                                    "name": tool_name,
                                    "response": {
                                        "output": tool_result_content_to_string(&tool_result.content),
                                    }
                                }
                            }));
                        }
                        _ => {}
                    }
                }

                if !parts.is_empty() {
                    result.push(serde_json::json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
            }
            Message::Assistant { content, .. } => {
                let mut parts = Vec::new();
                for item in content.iter() {
                    match item {
                        AssistantContent::Text(text) => {
                            parts.push(serde_json::json!({ "text": text.text }));
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            tool_call_name_by_id
                                .insert(tool_call.id.clone(), tool_call.function.name.clone());
                            parts.push(serde_json::json!({
                                "functionCall": {
                                    "name": tool_call.function.name,
                                    "args": tool_call.function.arguments,
                                }
                            }));
                        }
                        _ => {}
                    }
                }

                if !parts.is_empty() {
                    result.push(serde_json::json!({
                        "role": "model",
                        "parts": parts,
                    }));
                }
            }
        }
    }

    result
}

fn parse_sse_events(response_text: &str) -> Vec<serde_json::Value> {
    response_text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("data:") {
                return None;
            }
            let payload = trimmed.trim_start_matches("data:").trim();
            if payload.is_empty() || payload == "[DONE]" {
                return None;
            }
            serde_json::from_str(payload).ok()
        })
        .collect()
}

// --- Image conversion helpers ---

/// Convert a rig Image to an Anthropic image content block.
/// Anthropic format: {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "..."}}
fn convert_image_anthropic(image: &Image) -> Option<serde_json::Value> {
    let media_type = image
        .media_type
        .as_ref()
        .map(|mt| mt.to_mime_type())
        .unwrap_or("image/jpeg");

    match &image.data {
        DocumentSourceKind::Base64(data) => Some(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        })),
        DocumentSourceKind::Url(url) => Some(serde_json::json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url,
            }
        })),
        _ => None,
    }
}

/// Convert a rig Image to an OpenAI image_url content part.
/// OpenAI/OpenRouter format: {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,..."}}
fn convert_image_openai(image: &Image) -> Option<serde_json::Value> {
    let media_type = image
        .media_type
        .as_ref()
        .map(|mt| mt.to_mime_type())
        .unwrap_or("image/jpeg");

    match &image.data {
        DocumentSourceKind::Base64(data) => {
            let data_url = format!("data:{media_type};base64,{data}");
            Some(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": data_url }
            }))
        }
        DocumentSourceKind::Url(url) => Some(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": url }
        })),
        _ => None,
    }
}

fn convert_image_openai_responses(image: &Image) -> Option<serde_json::Value> {
    let media_type = image
        .media_type
        .as_ref()
        .map(|mime_type| mime_type.to_mime_type())
        .unwrap_or("image/jpeg");

    match &image.data {
        DocumentSourceKind::Base64(data) => {
            let data_url = format!("data:{media_type};base64,{data}");
            Some(serde_json::json!({
                "type": "input_image",
                "image_url": data_url,
            }))
        }
        DocumentSourceKind::Url(url) => Some(serde_json::json!({
            "type": "input_image",
            "image_url": url,
        })),
        _ => None,
    }
}

/// Truncate a response body for error messages to avoid dumping megabytes of HTML.
fn truncate_body(body: &str) -> &str {
    let limit = 500;
    if body.len() <= limit {
        body
    } else {
        &body[..limit]
    }
}

// --- Response parsing ---

fn make_tool_call(id: String, name: String, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id,
        call_id: None,
        function: ToolFunction {
            name: name.trim().to_string(),
            arguments,
        },
        signature: None,
        additional_params: None,
    }
}

fn parse_anthropic_response(
    body: serde_json::Value,
) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
    let content_blocks = body["content"]
        .as_array()
        .ok_or_else(|| CompletionError::ResponseError("missing content array".into()))?;

    let mut assistant_content = Vec::new();

    for block in content_blocks {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().unwrap_or("").to_string();
                assistant_content.push(AssistantContent::Text(Text { text }));
            }
            Some("tool_use") => {
                let id = block["id"].as_str().unwrap_or("").to_string();
                let name = block["name"].as_str().unwrap_or("").to_string();
                let arguments = block["input"].clone();
                assistant_content.push(AssistantContent::ToolCall(make_tool_call(
                    id, name, arguments,
                )));
            }
            Some("thinking") => {
                // Thinking blocks contain internal reasoning, not actionable output.
                // We'll skip them but log for debugging.
                tracing::debug!("skipping thinking block in Anthropic response");
            }
            _ => {
                // Unknown block type - log but skip
                tracing::debug!(
                    "skipping unknown block type in Anthropic response: {:?}",
                    block["type"].as_str()
                );
            }
        }
    }

    let choice = OneOrMany::many(assistant_content).unwrap_or_else(|_| {
        // Anthropic returns an empty content array when stop_reason is end_turn
        // and the model has nothing further to say (e.g. after a side-effect-only
        // tool call like react/skip). Treat this as a clean empty response rather
        // than an error so the agentic loop terminates gracefully.
        let stop_reason = body["stop_reason"].as_str().unwrap_or("unknown");
        tracing::debug!(
            stop_reason,
            content_blocks = content_blocks.len(),
            "empty assistant_content from Anthropic — returning synthetic empty text"
        );
        OneOrMany::one(AssistantContent::Text(Text {
            text: String::new(),
        }))
    });

    let input_tokens = body["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = body["usage"]["output_tokens"].as_u64().unwrap_or(0);
    let cached = body["usage"]["cache_read_input_tokens"]
        .as_u64()
        .unwrap_or(0);

    Ok(completion::CompletionResponse {
        choice,
        usage: completion::Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            cached_input_tokens: cached,
        },
        raw_response: RawResponse { body },
    })
}

fn parse_antigravity_response(
    events: Vec<serde_json::Value>,
) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
    let mut text_segments: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut input_tokens = 0;
    let mut output_tokens = 0;

    for event in &events {
        let response = event.get("response").unwrap_or(event);

        if let Some(candidates) = response["candidates"].as_array() {
            for candidate in candidates {
                if let Some(parts) = candidate["content"]["parts"].as_array() {
                    for part in parts {
                        if let Some(text) = part["text"].as_str()
                            && !text.is_empty()
                            && text_segments.last().map(String::as_str) != Some(text)
                        {
                            text_segments.push(text.to_string());
                        }

                        if let Some(function_call) = part["functionCall"].as_object() {
                            let name = function_call
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let arguments = function_call
                                .get("args")
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({}));
                            let already_present = tool_calls.iter().any(|call| {
                                call.function.name == name && call.function.arguments == arguments
                            });
                            if !already_present {
                                tool_calls.push(make_tool_call(
                                    format!("call_{}", uuid::Uuid::new_v4()),
                                    name,
                                    arguments,
                                ));
                            }
                        }
                    }
                }
            }
        }

        input_tokens = response["usageMetadata"]["promptTokenCount"]
            .as_u64()
            .unwrap_or(input_tokens);
        output_tokens = response["usageMetadata"]["candidatesTokenCount"]
            .as_u64()
            .unwrap_or(output_tokens);
    }

    let mut assistant_content = Vec::new();
    if !text_segments.is_empty() {
        assistant_content.push(AssistantContent::Text(Text {
            text: text_segments.join(""),
        }));
    }
    for tool_call in tool_calls {
        assistant_content.push(AssistantContent::ToolCall(tool_call));
    }

    let choice = OneOrMany::many(assistant_content)
        .map_err(|_| CompletionError::ResponseError("empty response from Antigravity".into()))?;
    let raw_body = serde_json::json!({ "events": events });

    Ok(completion::CompletionResponse {
        choice,
        usage: completion::Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            cached_input_tokens: 0,
        },
        raw_response: RawResponse { body: raw_body },
    })
}

fn parse_openai_response(
    body: serde_json::Value,
    provider_label: &str,
) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
    let choice = &body["choices"][0]["message"];

    let mut assistant_content = Vec::new();

    if let Some(text) = choice["content"].as_str()
        && !text.is_empty()
    {
        assistant_content.push(AssistantContent::Text(Text {
            text: text.to_string(),
        }));
    }

    // Some reasoning models (e.g., NVIDIA kimi-k2.5) return reasoning in a separate field
    if assistant_content.is_empty()
        && let Some(reasoning) = choice["reasoning_content"].as_str()
        && !reasoning.is_empty()
    {
        tracing::debug!(
            provider = %provider_label,
            "extracted reasoning_content as main content"
        );
        assistant_content.push(AssistantContent::Text(Text {
            text: reasoning.to_string(),
        }));
    }

    if let Some(reasoning_content) = choice["reasoning_content"].as_str() {
        if !reasoning_content.is_empty() {
            assistant_content.push(AssistantContent::Reasoning(rig::message::Reasoning::new(
                reasoning_content,
            )));
        }
    } else if let Some(reasoning_parts) = choice["reasoning_content"].as_array() {
        let reasoning: Vec<String> = reasoning_parts
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect();
        if !reasoning.is_empty() {
            assistant_content.push(AssistantContent::Reasoning(rig::message::Reasoning::multi(
                reasoning,
            )));
        }
    }

    if let Some(tool_calls) = choice["tool_calls"].as_array() {
        for tc in tool_calls {
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            // OpenAI-compatible APIs usually return arguments as a JSON string.
            // Some providers return it as a raw JSON object instead.
            let arguments_field = &tc["function"]["arguments"];
            let arguments = arguments_field
                .as_str()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .or_else(|| arguments_field.as_object().map(|_| arguments_field.clone()))
                .unwrap_or(serde_json::json!({}));
            assistant_content.push(AssistantContent::ToolCall(make_tool_call(
                id, name, arguments,
            )));
        }
    }

    let result_choice = OneOrMany::many(assistant_content.clone()).map_err(|_| {
        tracing::warn!(
            provider = %provider_label,
            choice = ?choice,
            "empty response from provider"
        );
        CompletionError::ResponseError(format!("empty response from {provider_label}"))
    })?;

    let input_tokens = body["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let output_tokens = body["usage"]["completion_tokens"].as_u64().unwrap_or(0);
    let cached = body["usage"]["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);

    Ok(completion::CompletionResponse {
        choice: result_choice,
        usage: completion::Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            cached_input_tokens: cached,
        },
        raw_response: RawResponse { body },
    })
}

fn parse_openai_responses_response(
    body: serde_json::Value,
) -> Result<completion::CompletionResponse<RawResponse>, CompletionError> {
    let output_items = body["output"]
        .as_array()
        .ok_or_else(|| CompletionError::ResponseError("missing output array".into()))?;

    let mut assistant_content = Vec::new();

    for output_item in output_items {
        match output_item["type"].as_str() {
            Some("message") => {
                if let Some(content_items) = output_item["content"].as_array() {
                    for content_item in content_items {
                        if content_item["type"].as_str() == Some("output_text")
                            && let Some(text) = content_item["text"].as_str()
                            && !text.is_empty()
                        {
                            assistant_content.push(AssistantContent::Text(Text {
                                text: text.to_string(),
                            }));
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = output_item["call_id"]
                    .as_str()
                    .or_else(|| output_item["id"].as_str())
                    .unwrap_or("")
                    .to_string();
                let name = output_item["name"].as_str().unwrap_or("").to_string();
                let arguments = output_item["arguments"]
                    .as_str()
                    .and_then(|arguments| serde_json::from_str(arguments).ok())
                    .unwrap_or(serde_json::json!({}));

                assistant_content.push(AssistantContent::ToolCall(make_tool_call(
                    call_id, name, arguments,
                )));
            }
            _ => {}
        }
    }

    let choice = OneOrMany::many(assistant_content).map_err(|_| {
        CompletionError::ResponseError("empty response from OpenAI Responses API".into())
    })?;

    let input_tokens = body["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = body["usage"]["output_tokens"].as_u64().unwrap_or(0);
    let cached = body["usage"]["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);

    Ok(completion::CompletionResponse {
        choice,
        usage: completion::Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            cached_input_tokens: cached,
        },
        raw_response: RawResponse { body },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::message::Reasoning;

    #[test]
    fn reverse_map_restores_original_tool_names() {
        let original_tools = vec![
            ("my_read".to_string(), "reads files".to_string()),
            ("my_bash".to_string(), "runs commands".to_string()),
        ];

        let mut completion = completion::CompletionResponse {
            choice: OneOrMany::one(AssistantContent::ToolCall(ToolCall {
                id: "tc1".into(),
                call_id: None,
                function: ToolFunction {
                    name: "My_Read".into(),
                    arguments: serde_json::json!({}),
                },
                signature: None,
                additional_params: None,
            })),
            usage: completion::Usage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cached_input_tokens: 0,
            },
            raw_response: RawResponse {
                body: serde_json::json!({}),
            },
        };

        reverse_map_tool_names(&mut completion, &original_tools);

        let first = completion.choice.first_ref();
        if let AssistantContent::ToolCall(tc) = first {
            assert_eq!(tc.function.name, "my_read");
        } else {
            panic!("expected ToolCall");
        }
    }

    #[test]
    fn convert_messages_to_openai_adds_empty_reasoning_content_for_kimi_tool_calls() {
        let assistant_content = OneOrMany::many(vec![AssistantContent::ToolCall(make_tool_call(
            "call_1".to_string(),
            "shell".to_string(),
            serde_json::json!({"command": "ls"}),
        ))])
        .expect("assistant content should build");
        let messages = OneOrMany::many(vec![Message::Assistant {
            id: None,
            content: assistant_content,
        }])
        .expect("messages should build");

        let converted = convert_messages_to_openai(&messages, true);

        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["reasoning_content"], "");
        assert!(converted[0]["tool_calls"].is_array());
    }

    #[test]
    fn convert_messages_to_openai_preserves_reasoning_content_for_kimi_tool_calls() {
        let assistant_content = OneOrMany::many(vec![
            AssistantContent::Reasoning(Reasoning::new("first")),
            AssistantContent::Reasoning(Reasoning::new("second")),
            AssistantContent::ToolCall(make_tool_call(
                "call_1".to_string(),
                "shell".to_string(),
                serde_json::json!({"command": "ls"}),
            )),
        ])
        .expect("assistant content should build");
        let messages = OneOrMany::many(vec![Message::Assistant {
            id: None,
            content: assistant_content,
        }])
        .expect("messages should build");

        let converted = convert_messages_to_openai(&messages, true);

        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["reasoning_content"], "first\nsecond");
    }

    #[test]
    fn parse_openai_response_extracts_reasoning_content() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "reasoning_content": "plan it",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                }
            }],
            "usage": {}
        });

        let parsed = parse_openai_response(body, "Test").expect("response should parse");
        let mut saw_reasoning = false;
        let mut saw_tool_call = false;

        for item in parsed.choice.iter() {
            match item {
                AssistantContent::Reasoning(reasoning) => {
                    saw_reasoning = true;
                    assert_eq!(reasoning.reasoning, vec!["plan it".to_string()]);
                }
                AssistantContent::ToolCall(tool_call) => {
                    saw_tool_call = true;
                    assert_eq!(tool_call.function.name, "shell");
                }
                _ => {}
            }
        }

        assert!(saw_reasoning);
        assert!(saw_tool_call);
    }

    #[test]
    fn antigravity_model_candidates_promote_sonnet_4_5_to_4_6() {
        let candidates = antigravity_model_candidates("claude-sonnet-4-5");
        assert_eq!(candidates[0], "claude-sonnet-4-6");
        assert!(candidates.iter().any(|model| model == "claude-sonnet-4-6"));
        assert!(candidates.iter().any(|model| model == "claude-sonnet-4-5"));
    }

    #[test]
    fn antigravity_model_candidates_map_sonnet_4_6_thinking_alias() {
        let candidates = antigravity_model_candidates("claude-sonnet-4-6-thinking");
        assert_eq!(candidates[0], "claude-sonnet-4-6-thinking");
        assert_eq!(candidates[1], "claude-sonnet-4-6");
    }

    #[test]
    fn antigravity_requires_claude_thinking_header_for_sonnet_4_6() {
        assert!(antigravity_requires_claude_thinking_header(
            "claude-sonnet-4-6"
        ));
        assert!(antigravity_requires_claude_thinking_header(
            "claude-opus-4-6-thinking"
        ));
        assert!(!antigravity_requires_claude_thinking_header(
            "gemini-3-flash"
        ));
    }

    #[test]
    fn antigravity_model_retry_only_triggers_for_model_errors() {
        assert!(should_try_next_antigravity_model(
            reqwest::StatusCode::BAD_REQUEST,
            "Requested entity was not found"
        ));
        assert!(!should_try_next_antigravity_model(
            reqwest::StatusCode::UNAUTHORIZED,
            "OAuth 2 credentials are invalid"
        ));
    }
}
