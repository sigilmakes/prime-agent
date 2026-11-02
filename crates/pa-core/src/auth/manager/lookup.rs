//! The API-key lookup + OAuth refresh arm: the resolution walk over the candidate sources (runtime
//! override, prime inference env-before-stored, stored, environment, fallback) with the staleness
//! gate.

use super::{
    now_epoch_ms, parse_storage_data, resolve_config_value, resolve_config_value_uncached,
    AuthApiKeyResult, AuthCredential, AuthStorage, PRIME_INFERENCE_PROVIDER_ID,
};

impl AuthStorage {
    /// Resolve the winning local credential without commands, OAuth adapters or fallback resolvers.
    /// Only an unexpired Codex OAuth access token can be used without its adapter.
    pub fn get_api_key_local(&self, provider_id: &str) -> Option<String> {
        if let Some(candidate) = self.runtime_candidate(provider_id) {
            if !self.is_stale(provider_id, &candidate) {
                return self.runtime_overrides.get(provider_id).cloned();
            }
        }
        let env_candidate = self.environment_candidate(provider_id);
        if provider_id == PRIME_INFERENCE_PROVIDER_ID {
            if let Some(candidate) = &env_candidate {
                if !self.is_stale(provider_id, candidate) {
                    return self.env_credentials.api_key(provider_id);
                }
            }
        }
        if let Some(credential) = self.data.credential(provider_id) {
            // Even fingerprinting a command-backed candidate executes it. A local
            // lookup cannot establish a different value or skip it for a lower source.
            if matches!(&credential, AuthCredential::ApiKey { key, .. } if key.starts_with('!')) {
                return None;
            }
            let candidate = match &credential {
                AuthCredential::Oauth {
                    access,
                    refresh,
                    expires,
                    ..
                } => {
                    let value_fingerprint = (provider_id == "openai-codex").then(|| {
                        super::fingerprint(
                            super::AuthSource::Stored,
                            &format!(
                                "value:auth.json\0oauth:{access}\0{}\0{expires}",
                                refresh.as_deref().unwrap_or_default()
                            ),
                        )
                    });
                    super::AuthSourceCandidate {
                        source: super::AuthSource::Stored,
                        configured: true,
                        label: None,
                        identity_fingerprint: super::fingerprint(
                            super::AuthSource::Stored,
                            "identity:auth.json",
                        ),
                        value_fingerprint,
                        resolve_value_fingerprint: None,
                    }
                }
                AuthCredential::ApiKey { .. } | AuthCredential::McpStaticToken { .. } => {
                    self.stored_candidate(provider_id)?
                }
            };
            let stale = if candidate.value_fingerprint.is_some() {
                self.is_stale(provider_id, &candidate)
            } else {
                !self.matching_stale(provider_id, &candidate).is_empty()
            };
            if !stale {
                return match credential {
                    AuthCredential::ApiKey { key, .. } => resolve_config_value(&key),
                    AuthCredential::Oauth {
                        access, expires, ..
                    } => (provider_id == "openai-codex" && now_epoch_ms() < expires)
                        .then_some(access),
                    AuthCredential::McpStaticToken { bearer, .. } => Some(bearer),
                };
            }
        }
        if provider_id != PRIME_INFERENCE_PROVIDER_ID {
            if let Some(candidate) = env_candidate {
                if !self.is_stale(provider_id, &candidate) {
                    return self.env_credentials.api_key(provider_id);
                }
            }
        }
        None
    }

    pub fn get_api_key_with_source_token(
        &mut self,
        provider_id: &str,
        include_fallback: bool,
    ) -> AuthApiKeyResult {
        // 1. Runtime override.
        if let Some(candidate) = self.runtime_candidate(provider_id) {
            if !self.is_stale(provider_id, &candidate) {
                if let Some(api_key) = self.runtime_overrides.get(provider_id).cloned() {
                    return AuthApiKeyResult {
                        api_key: Some(api_key),
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: Some("api_key"),
                    };
                }
            }
        }

        let env_key = self.env_credentials.api_key(provider_id);
        let env_candidate = self.environment_candidate(provider_id);

        // 2. Prime-inference: environment before stored.
        if provider_id == PRIME_INFERENCE_PROVIDER_ID {
            if let (Some(api_key), Some(candidate)) = (env_key.clone(), env_candidate.clone()) {
                if !self.is_stale(provider_id, &candidate) {
                    return AuthApiKeyResult {
                        api_key: Some(api_key),
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: Some("api_key"),
                    };
                }
            }
        }

        // 3. Stored credential.
        if let Some(credential) = self.data.credential(provider_id) {
            if let Some(candidate) = self.stored_candidate(provider_id) {
                if !self.is_stale(provider_id, &candidate) {
                    match &credential {
                        AuthCredential::ApiKey { key, .. } => {
                            let has_stale_record =
                                !self.matching_stale(provider_id, &candidate).is_empty();
                            let api_key = if key.starts_with('!') && has_stale_record {
                                resolve_config_value_uncached(key)
                            } else {
                                resolve_config_value(key)
                            };
                            return AuthApiKeyResult {
                                api_key,
                                source_token: Self::token_for(provider_id, &candidate),
                                credential_type: Some("api_key"),
                            };
                        }
                        AuthCredential::Oauth { expires, .. } => {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(i64::MAX, |d| d.as_millis() as i64);
                            if now_ms >= *expires {
                                if let Some(refreshed) = self.refresh_oauth(provider_id) {
                                    let candidate = self.stored_candidate(provider_id);
                                    return AuthApiKeyResult {
                                        api_key: self.oauth.api_key_for(provider_id, &refreshed),
                                        source_token: candidate
                                            .and_then(|c| Self::token_for(provider_id, &c)),
                                        credential_type: Some("oauth"),
                                    };
                                }
                                // Refresh failed: keep credentials for a
                                // later retry; discovery skips the provider.
                                return AuthApiKeyResult::default();
                            }
                            return AuthApiKeyResult {
                                api_key: self.oauth.api_key_for(provider_id, &credential),
                                source_token: Self::token_for(provider_id, &candidate),
                                credential_type: Some("oauth"),
                            };
                        }
                        // A pasted MCP static token IS the api key for its
                        // `mcp:<server>` provider: the bearer value, used verbatim.
                        AuthCredential::McpStaticToken { bearer, .. } => {
                            return AuthApiKeyResult {
                                api_key: Some(bearer.clone()),
                                source_token: Self::token_for(provider_id, &candidate),
                                credential_type: Some("mcp_static_token"),
                            };
                        }
                    }
                }
            }
        }

        // 4. Environment for non-prime-inference providers.
        if provider_id != PRIME_INFERENCE_PROVIDER_ID {
            if let (Some(api_key), Some(candidate)) = (env_key, env_candidate) {
                if !self.is_stale(provider_id, &candidate) {
                    return AuthApiKeyResult {
                        api_key: Some(api_key),
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: None,
                    };
                }
            }
        }

        // 5. Fallback resolver.
        if include_fallback {
            if let Some(candidate) = self.fallback_candidate(provider_id) {
                if !self.is_stale(provider_id, &candidate) {
                    let api_key = self
                        .fallback_resolver
                        .as_ref()
                        .and_then(|resolver| resolver(provider_id));
                    return AuthApiKeyResult {
                        api_key,
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: None,
                    };
                }
            }
        }

        AuthApiKeyResult::default()
    }

    pub fn get_api_key(&mut self, provider_id: &str) -> Option<String> {
        self.get_api_key_with_source_token(provider_id, true)
            .api_key
    }

    /// Refresh under the same cross-process document lock the TypeScript backend
    /// holds. A process-local flight gate cannot protect a single-use grant.
    fn refresh_oauth(&mut self, provider_id: &str) -> Option<AuthCredential> {
        if super::super::refresh_policy::disabled() {
            // A peer may already have committed a fresh token. Re-read the
            // authoritative file before refusing, but never invoke the refresh
            // integration or fall back to a repeatedly rejected old grant.
            self.reload();
            let current = self.data.credential(provider_id).filter(|credential| {
                matches!(credential, AuthCredential::Oauth { expires, .. } if now_epoch_ms() < *expires)
            });
            if current.is_none() {
                let message = "OAuth refresh disabled by PRIME_AGENT_DISABLE_OAUTH_REFRESH";
                if !self.errors.iter().any(|error| error == message) {
                    self.errors.push(message.into());
                }
            }
            return current;
        }
        let mut refreshed = None;
        let result = self.storage.with_refresh_lock(&mut |current| {
            let mut data = parse_storage_data(current.as_deref())?;
            let Some(credential) = data.credential(provider_id) else {
                return Ok(((), None));
            };
            let AuthCredential::Oauth { expires, .. } = &credential else {
                return Ok(((), None));
            };
            if now_epoch_ms() < *expires {
                refreshed = Some(credential);
                return Ok(((), None));
            }
            let Some(next) = self.oauth.refresh(provider_id, &data) else {
                return Ok(((), None));
            };
            // Only this locked snapshot may authorize spending the refresh token.
            // The file backend rechecks the entire raw document before publication,
            // refusing an out-of-protocol deletion/replacement during the fetch.
            data.insert_preserving_metadata(provider_id, &next);
            refreshed = Some(next);
            Ok(((), Some(serde_json::to_string_pretty(&data.0)?)))
        });
        if result.is_err() {
            self.reload();
            // A peer may have finished while lock acquisition waited. Never serve
            // an expired credential or the rejected fetch's credential on failure.
            return self.data.credential(provider_id).filter(|credential| {
                matches!(credential, AuthCredential::Oauth { expires, .. } if now_epoch_ms() < *expires)
            });
        }
        self.reload();
        refreshed
    }
}
