//! Reducer-owned navigation effect execution.
//!
//! This module deliberately owns only transport/effect orchestration and loaded
//! artifacts. All lifecycle mutations continue to flow through `ViewerModel`.

use super::*;
use crate::viewer::PageScope;

pub(super) enum RendererRecoveryEffect {
    EvictResource { message: String },
    EvictHistory { message: String },
    ReleaseHistoryArtifact { id: crate::viewer::HistoryId },
    RetirePageWork { scope: PageScope },
    ActivateErrorPage { message: String },
    RenderTerminal,
}

pub(super) enum ClassifiedEffect {
    Recovery(RendererRecoveryEffect),
    Other(LifecycleEffect),
}

pub(super) fn classify(effect: LifecycleEffect) -> ClassifiedEffect {
    match effect {
        LifecycleEffect::EvictResource { message } => {
            ClassifiedEffect::Recovery(RendererRecoveryEffect::EvictResource { message })
        }
        LifecycleEffect::EvictHistory { message } => {
            ClassifiedEffect::Recovery(RendererRecoveryEffect::EvictHistory { message })
        }
        LifecycleEffect::ReleaseHistoryArtifact { id } => {
            ClassifiedEffect::Recovery(RendererRecoveryEffect::ReleaseHistoryArtifact { id })
        }
        LifecycleEffect::RetirePageWork { scope } => {
            ClassifiedEffect::Recovery(RendererRecoveryEffect::RetirePageWork { scope })
        }
        LifecycleEffect::ActivateErrorPage { message } => {
            ClassifiedEffect::Recovery(RendererRecoveryEffect::ActivateErrorPage { message })
        }
        LifecycleEffect::RenderTerminal => {
            ClassifiedEffect::Recovery(RendererRecoveryEffect::RenderTerminal)
        }
        effect => ClassifiedEffect::Other(effect),
    }
}

/// Execute exactly one reducer-issued recovery effect. Completion events are
/// returned to the sole FIFO dispatcher; this function never reduces events.
pub(super) async fn execute_renderer_recovery(
    runtime: &mut TerminalRuntime,
    model: &LifecycleModel,
    effect: RendererRecoveryEffect,
) -> Result<Vec<LifecycleEvent>, ViewerError> {
    match effect {
        RendererRecoveryEffect::EvictResource { message } => {
            let evicted = runtime
                .client
                .as_mut()
                .is_some_and(AtpClient::evict_oldest_resource);
            Ok(vec![LifecycleEvent::ResourceEvictionCompleted {
                message,
                evicted,
            }])
        }
        RendererRecoveryEffect::EvictHistory { message } => {
            Ok(vec![LifecycleEvent::HistoryEvictionRequested { message }])
        }
        RendererRecoveryEffect::ReleaseHistoryArtifact { id } => {
            runtime.history.retain(|artifact| artifact.id != id);
            Ok(Vec::new())
        }
        RendererRecoveryEffect::RetirePageWork { scope } => {
            if let Some(client) = runtime.client.as_mut() {
                client.retire_page_work(&scope).await;
            }
            if runtime
                .prepared_layout
                .as_ref()
                .is_some_and(|(key, _)| key.generation == scope.generation)
            {
                runtime.prepared_layout = None;
            }
            runtime.prepared_wasm.clear_scope(&scope);
            retire_tick_attempt(&mut runtime.pending_tick_attempt, &scope);
            runtime.pending_updates.clear_scope(&scope);
            runtime.wasm_resources.clear_scope(&scope);
            runtime.fetched_pages.clear_scope(&scope);
            runtime.parsed_pages.clear_scope(&scope);
            runtime.prepared_navigation.clear_scope(&scope);
            if runtime
                .deferred_proposal
                .as_ref()
                .is_some_and(|proposal| proposal.generation == scope.generation)
            {
                runtime.deferred_proposal = None;
            }
            if runtime
                .deferred_navigation
                .as_ref()
                .is_some_and(|pending| pending.scope == scope)
            {
                runtime.deferred_navigation = None;
            }
            runtime.resumed_navigation.clear_scope(&scope);
            runtime.pending_redirect_depth = None;
            if runtime
                .pending_history_artifact
                .as_ref()
                .is_some_and(|(key, _)| key.generation == scope.generation)
            {
                runtime.pending_history_artifact = None;
            }
            runtime.activated_navigation.clear_scope(&scope);
            Ok(Vec::new())
        }
        RendererRecoveryEffect::ActivateErrorPage { message } => {
            let doc = parse_aml(&client_error_aml(&message)).ok_or(ViewerError::ParseFailed)?;
            let mut page = layout_page_with_admission(
                &doc,
                runtime.state.term_w,
                runtime.state.term_h,
                runtime.color_support,
                runtime.wcfg,
                None,
                model
                    .current_uri
                    .as_ref()
                    .map(AtpUri::try_clone)
                    .transpose()
                    .map_err(|_| ViewerError::ParseFailed)?,
                None,
                None,
                None,
            )
            .await
            .map_err(|_| ViewerError::ParseFailed)?;
            page.client_owned_error = true;
            runtime.event_dispatcher = page
                .prepared_event_dispatcher
                .take()
                .ok_or(ViewerError::ParseFailed)?;
            let content_height = u32::from(page.buf.height);
            runtime.state = ViewportState::with_sticky(
                runtime.state.term_w,
                runtime.state.term_h,
                page.buf.height,
                &page.sticky_buf,
            );
            runtime.region_buffers.clear();
            runtime.page = page;
            runtime.compositor.invalidate_cache();
            runtime.compositor.invalidate_presented();
            Ok(vec![LifecycleEvent::ErrorPageActivated { content_height }])
        }
        RendererRecoveryEffect::RenderTerminal => {
            runtime.render_authorized = true;
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::compositor::layout::cell::CellStyle;
    use crate::protocol::message::{UpdateFlags, UpdateMessage};
    use crate::viewer::PageScope;

    #[test]
    fn serialized_navigation_slot_rejects_stale_overwrite_and_retires_exact_scope() {
        let uri = AtpUri::parse("atp://127.0.0.1/").unwrap();
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let origin = client.request_origin(&uri).unwrap();
        let current = OperationOwner::new(
            PageScope {
                origin: origin.clone(),
                generation: 8,
            },
            3,
        );
        let stale = OperationOwner::new(
            PageScope {
                origin,
                generation: 7,
            },
            2,
        );
        let mut slot = PreparedSlot::default();

        assert!(slot.try_store(&current, "current").is_ok());
        assert_eq!(slot.try_store(&stale, "stale"), Err("stale"));
        assert_eq!(slot.get(&current), Some(&"current"));
        assert!(slot.take(&stale).is_none());
        assert!(slot.take_for_scope(&stale.scope).is_none());
        slot.clear_scope(&stale.scope);
        assert_eq!(slot.get(&current), Some(&"current"));
        assert_eq!(slot.take_for_scope(&current.scope), Some("current"));
        assert!(slot.is_empty());
    }

    #[test]
    fn tick_attempt_retirement_clears_only_the_matching_generation() {
        let uri = AtpUri::parse("atp://127.0.0.1/").unwrap();
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let origin = client.request_origin(&uri).unwrap();
        let current_scope = PageScope {
            origin: origin.clone(),
            generation: 8,
        };
        let stale_scope = PageScope {
            origin,
            generation: 7,
        };
        let key = PreparedWorkKey {
            generation: current_scope.generation,
            request_id: 3,
        };
        let timestamp = std::time::Instant::now();
        let mut attempt = Some((Some(key), timestamp));

        retire_tick_attempt(&mut attempt, &stale_scope);
        assert_eq!(attempt, Some((Some(key), timestamp)));
        retire_tick_attempt(&mut attempt, &current_scope);
        assert!(attempt.is_none());

        let mut local_attempt = Some((None, timestamp));
        retire_tick_attempt(&mut local_attempt, &current_scope);
        assert_eq!(local_attempt, Some((None, timestamp)));
    }

    #[test]
    fn runtime_pending_update_slot_preserves_exact_owner_and_releases_lease() {
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/").unwrap();
        let scope = PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 7,
        };
        let owner = OperationOwner::new(scope.try_clone().unwrap(), 11);
        let stale = OperationOwner::new(scope.try_clone().unwrap(), 12);
        let region = SubscriptionRegionKey::from_placed_index(0).unwrap();
        let governor = ResourceGovernor::new();
        let make_update = || UpdateMessage {
            region: String::from("ticker"),
            content: String::from("value"),
            flags: UpdateFlags::default(),
        };
        let first = ScopedUpdate::for_test(&owner, make_update(), region, &governor);
        let retained = governor.used(ResourceCategory::PendingUpdates);
        let content_ptr = first.update.content.as_ptr();
        let mut slot = PreparedSlot::default();
        assert!(slot.try_store(&owner, first).is_ok());

        let collision = ScopedUpdate::for_test(&stale, make_update(), region, &governor);
        let rejected = slot.try_store(&stale, collision).unwrap_err();
        drop(rejected);
        assert_eq!(governor.used(ResourceCategory::PendingUpdates), retained);
        assert_eq!(
            slot.get(&owner).unwrap().update.content.as_ptr(),
            content_ptr
        );
        assert!(slot.take(&stale).is_none());

        let retained_update = slot.take(&owner).unwrap();
        assert_eq!(retained_update.update.content.as_ptr(), content_ptr);
        drop(retained_update);
        assert_eq!(governor.used(ResourceCategory::PendingUpdates), 0);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn stale_cached_activation_cannot_mutate_transport_or_runtime_slots() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let current_scope = PageScope {
            origin: origin.clone(),
            generation: 4,
        };
        client.activate_page_scope(current_scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let stale_owner = OperationOwner::new(current_scope.clone(), 9);
        let entry = crate::viewer::HistoryEntry {
            id: 7,
            scope: current_scope.clone(),
            uri,
            retained_aml: String::from("[page title=Stale][/page]"),
        };
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(current_scope.clone());

        let events = runtime
            .execute(
                LifecycleEffect::ActivateCachedHistory {
                    owner: stale_owner,
                    entry,
                },
                &model,
            )
            .await
            .unwrap();

        assert!(events.is_empty());
        assert_eq!(
            runtime.client.as_ref().unwrap().current_scope(),
            Some(&current_scope)
        );
        assert!(runtime.parsed_pages.is_empty());
        assert!(runtime.prepared_navigation.is_empty());
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn stale_same_scope_wasm_work_cannot_mutate_runtime_artifacts() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = PageScope {
            origin,
            generation: 6,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri),
            None,
        )
        .await;
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope.clone());
        let stale_owner = OperationOwner::new(scope, 77);

        assert!(
            runtime
                .execute(
                    LifecycleEffect::LoadWasm {
                        owner: stale_owner.clone(),
                        path: "/stale.wasm".into(),
                    },
                    &model,
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            runtime
                .execute(
                    LifecycleEffect::TickWasm {
                        owner: Some(stale_owner.clone()),
                    },
                    &model,
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert!(runtime.wasm_resources.is_empty());
        assert!(runtime.prepared_wasm.get(&stale_owner).is_none());
        assert!(runtime.pending_tick_attempt.is_none());
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn successful_page_activation_releases_prior_live_region_buffers() {
        let active = layout_page(
            parse_aml("[page title=Current][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;
        let candidate = layout_page(
            parse_aml("[page title=Candidate][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;
        let governor = ResourceGovernor::new();
        let mut runtime = TerminalRuntime::new(
            active,
            None,
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let uri = AtpUri::parse("atp://127.0.0.1/candidate").unwrap();
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let scope = PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 5,
        };
        let owner = OperationOwner::new(scope.clone(), 9);
        let mut mini = CellBuffer::new(2, 1);
        mini.put_char(0, 0, 'x', &CellStyle::default());
        assert!(runtime.region_buffers.update(
            SubscriptionRegionKey::from_placed_index(0).unwrap(),
            2,
            1,
            1,
            &governor,
            &mini,
            RegionBufferUpdate::Replace,
        ));
        assert!(governor.used(ResourceCategory::SceneCells) > 0);
        runtime.store_prepared_layout(&owner, PreparedLayout::Page(Box::new(candidate)));
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope);

        let _events = runtime
            .execute(LifecycleEffect::ActivateLayout { owner }, &model)
            .await
            .unwrap();
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Candidate"));
        assert_eq!(runtime.region_buffers.len(), 0);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
        assert_eq!(governor.count(ResourceCategory::SceneCells), 0);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn cached_history_admits_wasm_batch_before_publishing_loads() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/cached").unwrap();
        let scope = PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 7,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope.clone());
        let owner = match crate::compositor::terminal::dispatch_reducer_events(
            &mut model,
            [LifecycleEvent::WasmRequested {
                path: "/owner-probe.wasm".into(),
            }],
        )
        .pop_front()
        .expect("expected reducer-owned WASM request")
        {
            LifecycleEffect::LoadWasm { ref owner, .. } => owner.clone(),
            _ => panic!("expected reducer-owned WASM request"),
        };
        let entry = crate::viewer::HistoryEntry {
            id: 8,
            scope,
            uri,
            retained_aml: String::from(
                "[page title=Cached][animate id=fx src=\"/cached.wasm\" w=2 h=1/][/page]",
            ),
        };

        let events = runtime
            .execute(
                LifecycleEffect::ActivateCachedHistory {
                    owner: owner.clone(),
                    entry,
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [
                LifecycleEvent::WasmDependenciesDiscovered { paths, .. },
                LifecycleEvent::ParseCompleted { .. }
            ] if paths == &[String::from("/cached.wasm")]
        ));
        let batch = runtime
            .wasm_resources
            .get(&owner)
            .expect("cached parse must install its WASM admission batch");
        assert_eq!(batch.remaining_paths, 1);
        assert_eq!(batch.unassigned_path_bytes, "/cached.wasm".len());
        assert!(runtime.parsed_pages.get(&owner).is_some());
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn fixed_wasm_batch_moves_exact_path_and_body_leases_into_loaded_page() {
        let governor = ResourceGovernor::new();
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let scope = PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 9,
        };
        let first = OperationOwner::new(scope.try_clone().unwrap(), 10);
        let second = OperationOwner::new(scope, 11);
        let first_path = String::from("/one.wasm");
        let second_path = String::from("/two.wasm");
        let rejected_path = String::from("/missing.wasm");
        let admitted_bytes = first_path
            .capacity()
            .checked_add(second_path.capacity())
            .and_then(|bytes| bytes.checked_add(rejected_path.capacity()))
            .unwrap();
        let lease = governor
            .reserve(ResourceCategory::RemoteCollections, admitted_bytes)
            .unwrap();
        let mut batch = PreparedWasmBatch::admitted(Some(lease), 3, admitted_bytes);
        batch
            .try_store(
                &first,
                first_path,
                ScopedResource::for_test(&first, &[1, 2, 3], &governor),
            )
            .unwrap();
        batch
            .try_store(
                &second,
                second_path,
                ScopedResource::for_test(&second, &[4, 5], &governor),
            )
            .unwrap();
        let duplicate_owner = OperationOwner::new(second.scope.try_clone().unwrap(), 12);
        let duplicate_path = String::from("/one.wasm");
        let charged_before_duplicate = governor.used(ResourceCategory::RemoteCollections);
        let Err((duplicate_path, duplicate_resource)) = batch.try_store(
            &duplicate_owner,
            duplicate_path,
            ScopedResource::for_test(&duplicate_owner, &[9], &governor),
        ) else {
            panic!("duplicate WASM path must not replace an artifact");
        };
        assert!(batch.contains_path(&duplicate_path));
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            charged_before_duplicate
        );
        drop(duplicate_resource);
        batch.reject_path(&rejected_path);

        assert_eq!(batch.len(), 2);
        assert!(batch.contains_owner(&first));
        assert!(batch.contains_owner(&second));
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            admitted_bytes - rejected_path.capacity()
        );
        assert_eq!(governor.used(ResourceCategory::ResourceCache), 5);

        let mut page = layout_page(
            parse_aml("[page title=Loaded][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;
        page.prepared_wasm = batch;
        assert_eq!(page.prepared_wasm.len(), 2);
        assert_eq!(governor.used(ResourceCategory::ResourceCache), 5);
        drop(page);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
        assert_eq!(governor.used(ResourceCategory::ResourceCache), 0);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn renderer_recovery_evicts_non_current_history_before_error_activation() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = PageScope {
            origin,
            generation: 3,
        };
        client.activate_page_scope(scope.clone()).await;
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope.clone());
        model.current_uri = Some(uri.try_clone().unwrap());
        model.history = vec![
            crate::viewer::HistoryEntry {
                id: 1,
                scope: scope.clone(),
                uri: uri.try_clone().unwrap(),
                retained_aml: String::from("old"),
            },
            crate::viewer::HistoryEntry {
                id: 2,
                scope: scope.clone(),
                uri,
                retained_aml: String::from("current"),
            },
        ]
        .into_iter()
        .collect();
        model.history_position = Some(1);
        let mut lifecycle = ReducerPort::new(model);
        let doc = parse_aml("[page mode=document][text]current[/text][/page]").unwrap();
        let page = layout_page(
            doc,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            lifecycle
                .current_uri
                .as_ref()
                .map(AtpUri::try_clone)
                .transpose()
                .unwrap(),
            None,
        )
        .await;
        let old_lease = client
            .governor
            .reserve(ResourceCategory::History, 3)
            .unwrap();
        let current_lease = client
            .governor
            .reserve(ResourceCategory::History, 7)
            .unwrap();
        let history = vec![
            HistoryEntry {
                id: 1,
                _retained_bytes: 3,
                _budget_lease: Some(old_lease),
                title: "old".into(),
                transition: None,
                transition_duration_ms: 0,
            },
            HistoryEntry {
                id: 2,
                _retained_bytes: 7,
                _budget_lease: Some(current_lease),
                title: "current".into(),
                transition: None,
                transition_duration_ms: 0,
            },
        ];

        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            history,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );

        crate::compositor::terminal::dispatch_runtime_events(
            &mut runtime,
            &mut lifecycle,
            [LifecycleEvent::PresentationFailed {
                message: "budget exceeded".into(),
                retry: None,
            }],
        )
        .await
        .unwrap();

        assert_eq!(runtime.history.len(), 1);
        assert_eq!(runtime.history[0].id, 2);
        assert_eq!(lifecycle.history.len(), 1);
        assert_eq!(lifecycle.history[0].id, 2);
        assert_eq!(lifecycle.history_position, Some(0));
        assert_eq!(
            runtime
                .client
                .as_ref()
                .unwrap()
                .governor
                .used(ResourceCategory::History),
            7
        );
        assert!(!runtime.page.client_owned_error);
        assert!(runtime.render_authorized);
        assert_eq!(lifecycle.phase, crate::viewer::NavigationPhase::Idle);

        runtime.render_authorized = false;
        crate::compositor::terminal::dispatch_runtime_events(
            &mut runtime,
            &mut lifecycle,
            [LifecycleEvent::PresentationFailed {
                message: "budget exceeded".into(),
                retry: None,
            }],
        )
        .await
        .unwrap();

        assert!(runtime.page.client_owned_error);
        assert!(runtime.render_authorized);
        assert_eq!(lifecycle.phase, crate::viewer::NavigationPhase::Failed);
        assert_eq!(lifecycle.scope, Some(scope));
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn governed_layout_rejection_is_returned_without_error_substitution() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/").unwrap();
        let hostile_governor = client.governor.clone();
        let _pressure = hostile_governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let doc = parse_aml("[page mode=document][text]hostile[/text][/page]").unwrap();
        let result = layout_page_with_admission(
            &doc,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(result, Err(PagePreparationRejected { .. })));
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn history_admission_rejection_retains_exact_candidate_until_install() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/candidate").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let active = layout_page(
            parse_aml("[page title=Current][text]current[/text][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let candidate = layout_page(
            parse_aml("[page title=Candidate][text]candidate[/text][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let mut lifecycle = ReducerPort::new(LifecycleModel::new(40, 12));
        let effects = dispatch_event(
            &mut lifecycle,
            LifecycleEvent::Navigate {
                uri: uri.try_clone().unwrap(),
                origin,
            },
        );
        let owner = effects
            .into_iter()
            .find_map(|effect| match effect {
                LifecycleEffect::Fetch { owner, .. } => Some(owner),
                _ => None,
            })
            .unwrap();
        let aml = String::from("candidate");
        let aml_ptr = aml.as_ptr();
        let aml_len = aml.len();
        let admission = dispatch_event(
            &mut lifecycle,
            LifecycleEvent::HistoryAdmissionRequested {
                owner: owner.clone(),
                uri,
                retained_aml: aml,
            },
        );
        let effect = admission.into_iter().next().unwrap();
        let id = match &effect {
            LifecycleEffect::AdmitHistoryArtifact { id, .. } => *id,
            _ => panic!("expected admission effect"),
        };
        let mut runtime = TerminalRuntime::new(
            active,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let shared_title = candidate.scene.title.clone().unwrap();
        runtime.store_prepared_layout(&owner, PreparedLayout::Page(Box::new(candidate)));
        runtime.store_pending_history_artifact(
            &owner,
            PendingHistoryArtifact {
                id: None,
                retained_bytes: aml_len + shared_title.len(),
                budget_lease: None,
                title: shared_title.clone(),
                transition: None,
                transition_duration_ms: 0,
            },
        );
        let governor = runtime.client.as_ref().unwrap().governor.clone();

        // Refuse the history slot itself, with the budget untouched. Budget
        // pressure refuses the lease; naming the site refuses the slot
        // reservation that precedes it, which is the ordering that decides
        // whether a rejected entry can leave the candidate un-retained.
        {
            use crate::compositor::terminal::runner::{RunnerAllocationSite, RunnerRejectionGuard};
            let _rejection = RunnerRejectionGuard::at(RunnerAllocationSite::HistoryEntry);
            let refused = runtime
                .execute(
                    LifecycleEffect::AdmitHistoryArtifact {
                        owner: owner.clone(),
                        id,
                        replacing: false,
                    },
                    &lifecycle,
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    refused.as_slice(),
                    [LifecycleEvent::PresentationFailed { .. }]
                ),
                "a refused history slot must fail closed: {refused:?}"
            );
            assert!(lifecycle.history.is_empty());
            assert!(
                runtime.pending_history_artifact.is_some(),
                "the candidate must be retained for the retry"
            );
            assert_eq!(governor.used(ResourceCategory::History), 0);
        }

        let pressure = governor
            .reserve(ResourceCategory::History, 16 * 1024 * 1024)
            .unwrap();

        let rejected = runtime.execute(effect, &lifecycle).await.unwrap();
        assert!(matches!(
            rejected.as_slice(),
            [LifecycleEvent::PresentationFailed {
                retry: Some(PressureRetry::HistoryArtifact {
                    owner: retry_owner,
                    id: retry_id,
                    replacing: false,
                }),
                ..
            }] if retry_owner == &owner && *retry_id == id
        ));
        assert!(lifecycle.history.is_empty());
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Current"));
        assert!(
            runtime
                .pending_history_artifact
                .as_ref()
                .is_some_and(|(key, _)| *key == PreparedWorkKey::from(&owner))
        );
        assert!(Arc::ptr_eq(
            &runtime.pending_history_artifact.as_ref().unwrap().1.title,
            &shared_title
        ));

        drop(pressure);
        let admitted = runtime
            .execute(
                LifecycleEffect::AdmitHistoryArtifact {
                    owner: owner.clone(),
                    id,
                    replacing: false,
                },
                &lifecycle,
            )
            .await
            .unwrap();
        let install = dispatch_event(&mut lifecycle, admitted.into_iter().next().unwrap());
        assert_eq!(lifecycle.history.len(), 1);
        assert_eq!(lifecycle.history[0].id, id);
        assert_eq!(lifecycle.history[0].retained_aml.as_ptr(), aml_ptr);
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Current"));

        let prepared = runtime
            .execute(install.into_iter().last().unwrap(), &lifecycle)
            .await
            .unwrap();
        assert!(matches!(
            prepared.as_slice(),
            [LifecycleEvent::LayoutPrepared { owner: prepared_owner, .. }]
                if prepared_owner == &owner
        ));
        assert_eq!(runtime.history.len(), 1);
        assert_eq!(runtime.history[0].id, id);
        assert_eq!(
            governor.used(ResourceCategory::History),
            aml_len + shared_title.len()
        );
        assert!(Arc::ptr_eq(&runtime.history[0].title, &shared_title));
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Current"));
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn parse_pressure_retains_exact_payload_and_active_page_for_retry() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/candidate").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = PageScope {
            origin,
            generation: 2,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][text]current[/text][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let governor = client.governor.clone();
        let remaining = crate::resource::MAX_REMOTE_MEMORY - governor.total_used();
        let pressure = governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();
        let owner = OperationOwner::new(scope.clone(), 9);
        let aml = String::from("[page title=Candidate][text]candidate[/text][/page]");
        let aml_ptr = aml.as_ptr();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        assert!(runtime.fetched_pages.try_store(&owner, (uri, aml)).is_ok());
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope);

        let rejected = runtime
            .execute(
                LifecycleEffect::Parse {
                    owner: owner.clone(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                rejected.as_slice(),
                [LifecycleEvent::PresentationFailed {
                    retry: Some(PressureRetry::Parse { owner: retry_owner }),
                    ..
                }] if retry_owner == &owner
            ),
            "unexpected rejection: {rejected:?}"
        );
        assert_eq!(
            runtime.fetched_pages.get(&owner).unwrap().1.as_ptr(),
            aml_ptr
        );
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Current"));
        assert!(runtime.parsed_pages.is_empty());
        assert!(runtime.prepared_navigation.is_empty());

        drop(pressure);
        let completed = runtime
            .execute(
                LifecycleEffect::Parse {
                    owner: owner.clone(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            completed.as_slice(),
            [
                LifecycleEvent::WasmDependenciesDiscovered { .. },
                LifecycleEvent::ParseCompleted { .. }
            ]
        ));
        assert!(!runtime.fetched_pages.contains_key(&owner));
        assert_eq!(
            runtime
                .parsed_pages
                .get(&owner)
                .unwrap()
                .aml_content
                .as_ref()
                .unwrap()
                .as_ptr(),
            aml_ptr
        );
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Current"));
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn wasm_batch_path_pressure_restores_exact_parse_candidate_for_retry() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/candidate").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = PageScope {
            origin,
            generation: 3,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][text]current[/text][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let owner = OperationOwner::new(scope.clone(), 10);
        let aml = String::from(
            "[page title=Candidate][animate id=fx src=\"/effect.wasm\" w=2 h=1/][/page]",
        );
        let aml_ptr = aml.as_ptr();
        let governor = client.governor.clone();
        let remote_baseline = governor.used(ResourceCategory::RemoteCollections);
        let parse_bytes = aml.len() * PARSE_TRANSIENT_MULTIPLIER;
        let remaining = crate::resource::MAX_REMOTE_MEMORY - governor.total_used();
        let pressure = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                remaining.saturating_sub(parse_bytes + 4),
            )
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        assert!(runtime.fetched_pages.try_store(&owner, (uri, aml)).is_ok());
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope);

        let rejected = runtime
            .execute(
                LifecycleEffect::Parse {
                    owner: owner.clone(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                rejected.as_slice(),
                [LifecycleEvent::PresentationFailed {
                    retry: Some(PressureRetry::Parse { owner: retry_owner }),
                    ..
                }] if retry_owner == &owner
            ),
            "unexpected WASM batch rejection: {rejected:?}"
        );
        assert_eq!(
            runtime.fetched_pages.get(&owner).unwrap().1.as_ptr(),
            aml_ptr
        );
        assert!(runtime.parsed_pages.is_empty());
        assert!(runtime.wasm_resources.is_empty());

        drop(pressure);

        // Refuse the batch slot itself with the budget free. Pressure refuses
        // the path lease that precedes the slot, so it cannot show that a
        // failure to *store* the admitted batch still restores the fetched
        // page exactly rather than stranding its bytes.
        {
            use crate::compositor::terminal::runner::{RunnerAllocationSite, RunnerRejectionGuard};
            let before = governor.used(ResourceCategory::RemoteCollections);
            let _rejection = RunnerRejectionGuard::at(RunnerAllocationSite::WasmBatch);
            let refused = runtime
                .execute(
                    LifecycleEffect::Parse {
                        owner: owner.clone(),
                    },
                    &model,
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    refused.as_slice(),
                    [LifecycleEvent::PresentationFailed { .. }]
                ),
                "a refused batch slot must fail closed: {refused:?}"
            );
            assert_eq!(
                runtime.fetched_pages.get(&owner).unwrap().1.as_ptr(),
                aml_ptr,
                "the fetched page must be restored byte-for-byte"
            );
            assert!(runtime.wasm_resources.is_empty());
            assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);
        }

        let completed = runtime
            .execute(
                LifecycleEffect::Parse {
                    owner: owner.clone(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            completed.as_slice(),
            [
                LifecycleEvent::WasmDependenciesDiscovered { paths, .. },
                LifecycleEvent::ParseCompleted { .. }
            ] if paths == &[String::from("/effect.wasm")]
        ));
        assert!(runtime.wasm_resources.get(&owner).is_some());
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            remote_baseline + "/effect.wasm".len()
        );
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn malformed_aml_and_authored_invalid_wasm_are_not_pressure_rejections() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/candidate").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = PageScope {
            origin,
            generation: 3,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let owner = OperationOwner::new(scope.clone(), 10);
        let malformed = String::from("[text]not a page[/text]");
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        assert!(
            runtime
                .fetched_pages
                .try_store(&owner, (uri.try_clone().unwrap(), malformed))
                .is_ok()
        );
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope);
        let invalid = runtime
            .execute(
                LifecycleEffect::Parse {
                    owner: owner.clone(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            invalid.as_slice(),
            [LifecycleEvent::ParseFailed { .. }]
        ));

        let document = parse_aml("[page][animate id=bad src=/bad.wasm w=2 h=1/][/page]").unwrap();
        let mut prepared = HashMap::new();
        prepared.insert("/bad.wasm".into(), Arc::<[u8]>::from([0xff, 0x00]));
        let candidate = layout_page_with_admission(
            &document,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            runtime.client.as_mut(),
            Some(uri),
            None,
            None,
            Some(&prepared),
        )
        .await;
        assert!(candidate.is_ok());
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn layout_pressure_retains_exact_parsed_candidate_until_activation() {
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/candidate").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = PageScope {
            origin,
            generation: 4,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][text]current[/text][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let source = "[page title=Candidate][animate id=fx src=\"/effects/typewriter.wasm\" w=8 h=1][text]candidate[/text][/animate][/page]";
        let mut aml = String::with_capacity(source.len() + 128);
        aml.push_str(source);
        let aml_ptr = aml.as_ptr();
        let aml_capacity = aml.capacity();
        let (document, parse_lease) = parse_remote_aml(&aml, &client.governor).unwrap();
        let governor = client.governor.clone();
        let owner = OperationOwner::new(scope.clone(), 11);
        let resource_owner = OperationOwner::new(scope.try_clone().unwrap(), 12);
        let wasm_path = String::from("/effects/typewriter.wasm");
        let wasm_path_bytes = wasm_path.capacity();
        let wasm_path_lease = governor
            .reserve(ResourceCategory::RemoteCollections, wasm_path_bytes)
            .unwrap();
        let wasm_bytes =
            include_bytes!("../../../../../tests/fixtures/site/effects/typewriter.wasm");
        let mut wasm_batch = PreparedWasmBatch::admitted(Some(wasm_path_lease), 1, wasm_path_bytes);
        wasm_batch
            .try_store(
                &resource_owner,
                wasm_path,
                ScopedResource::for_test(&resource_owner, wasm_bytes, &governor),
            )
            .unwrap();
        let remaining = crate::resource::MAX_REMOTE_MEMORY - governor.total_used();
        let pressure = governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        assert!(
            runtime
                .parsed_pages
                .try_store(
                    &owner,
                    ParsedPage {
                        document,
                        parse_lease,
                        final_uri: uri,
                        aml_content: Some(aml),
                        cached_entry: None,
                    },
                )
                .is_ok()
        );
        assert!(runtime.wasm_resources.try_store(&owner, wasm_batch).is_ok());
        let mut model = LifecycleModel::new(40, 12);
        model.scope = Some(scope);

        let rejected = runtime
            .execute(
                LifecycleEffect::PrepareLayout {
                    owner: owner.clone(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            rejected.as_slice(),
            [LifecycleEvent::PresentationFailed {
                retry: Some(PressureRetry::PrepareLayout { owner: retry_owner }),
                ..
            }] if retry_owner == &owner
        ));
        assert_eq!(
            runtime
                .parsed_pages
                .get(&owner)
                .unwrap()
                .aml_content
                .as_ref()
                .unwrap()
                .as_ptr(),
            aml_ptr
        );
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Current"));
        assert!(runtime.prepared_layout.is_none());
        assert!(runtime.prepared_navigation.is_empty());
        assert_eq!(
            runtime
                .wasm_resources
                .get(&owner)
                .and_then(|batch| batch.path_lease.as_ref())
                .map(BudgetLease::amount),
            Some(wasm_path_bytes)
        );

        drop(pressure);
        let completed = runtime
            .execute(
                LifecycleEffect::PrepareLayout {
                    owner: owner.clone(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            completed.as_slice(),
            [LifecycleEvent::HistoryAdmissionRequested { retained_aml, .. }]
                if retained_aml.as_ptr() == aml_ptr
        ));
        assert_eq!(
            runtime
                .pending_history_artifact
                .as_ref()
                .unwrap()
                .1
                .retained_bytes,
            aml_capacity + "Candidate".len()
        );
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Current"));
        assert!(runtime.parsed_pages.is_empty());
        let Some(PreparedLayout::Page(page)) = runtime.prepared_layout(&owner) else {
            panic!("expected prepared page");
        };
        assert_eq!(
            page.prepared_wasm
                .path_lease
                .as_ref()
                .map(BudgetLease::amount),
            Some(wasm_path_bytes)
        );
        assert_eq!(page.prepared_wasm.len(), 1);
        assert!(page.prepared_wasm.contains_owner(&resource_owner));
        assert!(runtime.wasm_resources.is_empty());
        assert!(runtime.prepared_navigation.contains_key(&owner));
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn initial_parse_pressure_evicts_cache_then_completes_exact_candidate() {
        let mut model = LifecycleModel::new(40, 12);
        let uri = AtpUri::parse("atp://127.0.0.1/candidate").unwrap();
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let origin = client.request_origin(&uri).unwrap();
        let effects = crate::compositor::terminal::dispatch_event(
            &mut model,
            LifecycleEvent::Navigate {
                uri: uri.try_clone().unwrap(),
                origin: origin.clone(),
            },
        );
        let owner = match effects.last().unwrap() {
            LifecycleEffect::Fetch { owner, .. } => owner.clone(),
            _ => panic!("expected owned fetch"),
        };
        client.activate_page_scope(owner.scope.clone()).await;
        let page = layout_page(
            parse_aml("[page title=Current][text]current[/text][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        client
            .resource_cache
            .insert(origin, "/old".into(), vec![7; 1024 * 1024])
            .unwrap();
        let governor = client.governor.clone();
        let remaining = crate::resource::MAX_REMOTE_MEMORY - governor.total_used();
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();
        let aml = String::from("[page title=Candidate][text]candidate[/text][/page]");
        let aml_ptr = aml.as_ptr();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        assert!(
            runtime
                .fetched_pages
                .try_store(&owner, (uri.try_clone().unwrap(), aml))
                .is_ok()
        );
        let mut lifecycle = ReducerPort::new(model);

        crate::compositor::terminal::dispatch_runtime_events(
            &mut runtime,
            &mut lifecycle,
            [LifecycleEvent::FetchCompleted {
                owner: owner.clone(),
            }],
        )
        .await
        .unwrap();

        assert_eq!(lifecycle.phase, NavigationPhase::Ready);
        assert_eq!(lifecycle.history.len(), 1);
        assert_eq!(lifecycle.history[0].retained_aml.as_ptr(), aml_ptr);
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Candidate"));
        assert!(runtime.fetched_pages.is_empty());
        assert!(runtime.parsed_pages.is_empty());
        assert!(runtime.prepared_layout.is_none());
        assert_eq!(
            runtime
                .client
                .as_ref()
                .unwrap()
                .governor
                .used(ResourceCategory::ResourceCache),
            0
        );
        drop(blocker);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn cached_local_activation_consumes_the_fallibly_prepared_aml_without_cloning() {
        let page = layout_page(
            parse_aml("[page title=Current][text]current[/text][/page]").unwrap(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;
        let mut runtime = TerminalRuntime::new(
            page,
            None,
            Vec::new(),
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let model = LifecycleModel::new(40, 12);
        let mut aml = String::with_capacity(1024);
        aml.push_str("[page title=Cached][text]cached[/text][/page]");
        let aml_ptr = aml.as_ptr() as usize;

        let events = runtime
            .execute(
                LifecycleEffect::ApplyPresentationAction {
                    scope: None,
                    action: PresentationAction::ActivateLocalPage {
                        aml,
                        uri: None,
                        overlay: false,
                    },
                },
                &model,
            )
            .await
            .unwrap();

        assert!(events.is_empty());
        assert!(runtime.local_page_activated);
        assert_eq!(runtime.last_local_page_aml_ptr, Some(aml_ptr));
        assert_eq!(runtime.page.scene.title.as_deref(), Some("Cached"));
    }

    /// The reason the reducer supplies is what the page says. The regression
    /// this guards is not a missing string but a wrong one: the page used to
    /// be a `const` naming the resource budget, so a refused connection
    /// reported a limit that had not been reached.
    #[test]
    fn failure_page_reports_the_reason_it_was_given() {
        let aml = client_error_aml("atp://hub.example:1987/ — I/O error: Connection refused");

        assert!(aml.contains("Connection refused"), "{aml}");
        assert!(aml.contains("hub.example:1987"), "{aml}");
        assert!(
            !aml.contains("resource budget"),
            "the fixed budget wording must not survive: {aml}"
        );
        assert!(parse_aml(&aml).is_some(), "{aml}");
    }

    /// A server picks the text of an ERROR frame, so the detail is remote
    /// input on a page the client owns. It goes through `to_aml`, which means
    /// a forged tag arrives as characters rather than as markup on an origin
    /// the reader has no reason to distrust.
    #[test]
    fn hostile_detail_is_escaped_rather_than_parsed() {
        let aml = client_error_aml(r#"[form action="/login"][input name="password"][/form]"#);

        // Scanned rather than string-matched: `[[form` contains `[form`, so a
        // substring test passes whether or not the escape is there. What
        // matters is that no `form` tag comes back out.
        let tokens = dustnet_core::scanner::Scanner::new(aml.as_bytes())
            .unwrap()
            .scan_all()
            .unwrap();
        let tags: Vec<&str> = tokens
            .iter()
            .filter_map(|token| match token {
                dustnet_core::scanner::Token::OpenTag { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(tags, ["page", "text", "text"], "{aml}");
        assert!(
            tokens.iter().any(|token| matches!(
                token,
                dustnet_core::scanner::Token::Text(text)
                    if text.contains(r#"[form action="/login"]"#)
            )),
            "the forged tag must survive as text: {aml}"
        );
    }

    /// The page that reports a failure must not be able to become the next
    /// one. Truncation is by character rather than byte, so a server whose
    /// message is multi-byte cannot panic the client that renders it.
    #[test]
    fn overlong_detail_is_cut_on_a_character_boundary() {
        let detail = "é".repeat(MAX_ERROR_DETAIL * 4);
        let aml = client_error_aml(&detail);

        assert!(aml.contains(&"é".repeat(MAX_ERROR_DETAIL)), "{aml}");
        assert!(!aml.contains(&"é".repeat(MAX_ERROR_DETAIL + 1)), "{aml}");
        assert!(parse_aml(&aml).is_some(), "{aml}");
    }
}
