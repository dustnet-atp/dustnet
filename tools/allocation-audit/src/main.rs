//! Mechanical inventory check for remotely influenced heap owners.
//!
//! This dependency-free checker lexes named fields in an explicit list of
//! authority structs. The TSV stores the policy facts; this tool prevents
//! missing, duplicate, renamed, and stale rows.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const COLUMN_COUNT: usize = 14;
const HEADER: &str = "id\tsource\towner\tfield\tinfluence\tlifetime\tstorage\tbound\tcategory\tadmission\tfailure\trelease\tstatus\ttests";

const AUTHORITIES: &[(&str, &str)] = &[
    ("crates/dustnet-core/src/protocol/uri.rs", "AtpUri"),
    ("crates/dustnet-core/src/protocol/frame.rs", "RawFrame"),
    (
        "crates/dustnet-core/src/protocol/message.rs",
        "HelloMessage",
    ),
    (
        "crates/dustnet-core/src/protocol/message.rs",
        "WelcomeMessage",
    ),
    ("crates/dustnet-core/src/protocol/message.rs", "GetMessage"),
    ("crates/dustnet-core/src/protocol/message.rs", "PageMessage"),
    (
        "crates/dustnet-core/src/protocol/message.rs",
        "RedirectMessage",
    ),
    (
        "crates/dustnet-core/src/protocol/message.rs",
        "ErrorMessage",
    ),
    (
        "crates/dustnet-core/src/protocol/message.rs",
        "InputMessage",
    ),
    (
        "crates/dustnet-core/src/protocol/message.rs",
        "SubscribeMessage",
    ),
    (
        "crates/dustnet-core/src/protocol/message.rs",
        "UpdateMessage",
    ),
    ("crates/dustnet-core/src/session.rs", "SessionStore"),
    ("crates/dustnet-core/src/session.rs", "SiteSessionStore"),
    ("crates/dustnet-core/src/scanner/mod.rs", "Scanner"),
    ("crates/dustnet-core/src/parser/mod.rs", "Parser"),
    (
        "crates/dustnet-core/src/parser/components.rs",
        "ComponentDef",
    ),
    ("crates/dustnet-client/src/client.rs", "AtpClient"),
    (
        "crates/dustnet-client/src/client.rs",
        "PreparedConnectionOrigin",
    ),
    ("crates/dustnet-client/src/client.rs", "ResourceCache"),
    ("crates/dustnet-client/src/client.rs", "ResourceCacheEntry"),
    ("crates/dustnet-client/src/client.rs", "SharedResource"),
    (
        "crates/dustnet-client/src/client.rs",
        "SharedResourceAllocation",
    ),
    ("crates/dustnet-client/src/client.rs", "ScopedResource"),
    ("crates/dustnet-client/src/transport.rs", "AtpConnection"),
    (
        "crates/dustnet-client/src/session_store.rs",
        "GovernedSessionStore",
    ),
    ("crates/dustnet-client/src/viewer.rs", "ViewerModel"),
    ("crates/dustnet-client/src/viewer.rs", "HistoryEntry"),
    (
        "crates/dustnet-client/src/viewer.rs",
        "PendingHistoryActivation",
    ),
    (
        "crates/dustnet-client/src/viewer.rs",
        "PendingHistoryCommit",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "LoadedPage",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "TerminalRuntime",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "PreparedWasmArtifact",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "PreparedWasmBatch",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "RegionBuffer",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "RegionBufferEntry",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "RegionBuffers",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/tree.rs",
        "Scene",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/tree.rs",
        "SceneNodes",
    ),
    ("crates/dustnet-client/src/compositor/scene/node.rs", "Node"),
    (
        "crates/dustnet-client/src/compositor/scene/events.rs",
        "EventBinding",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/tree.rs",
        "RelayoutJournal",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/invalidation.rs",
        "Invalidation",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/invalidation.rs",
        "LayoutInvalidation",
    ),
    (
        "crates/dustnet-client/src/compositor/composite.rs",
        "Compositor",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/runtime.rs",
        "AnimationRuntime",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/effect.rs",
        "TextEffectAdapter",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/tween.rs",
        "TweenAdapter",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/frame.rs",
        "FrameAnimationAdapter",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/wasm.rs",
        "WasmAnimationAdapter",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/transition.rs",
        "TransitionAdapter",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/page_transition.rs",
        "PageTransitionAdapter",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/events.rs",
        "EventDispatcher",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/presentation.rs",
        "CommandLine",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/presentation.rs",
        "ErrorLog",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/presentation.rs",
        "ErrorEntry",
    ),
    ("crates/dustnet-server/src/transport.rs", "AtpServerStream"),
    ("crates/dustnet-server/src/lib.rs", "StaticSubscription"),
    ("crates/dustnet-server/src/live_watch.rs", "LiveGeneration"),
    ("crates/dustnet-server/src/live_watch.rs", "WatchedFile"),
    ("crates/dustnet-server/src/live_watch.rs", "WatchRegistry"),
];

/// Heap-producing functions that do not retain their result in a named field.
/// Each site has an explicit pseudo-field row in the inventory so these
/// allocations cannot disappear behind the struct-only scanner.
const TRANSIENT_SITES: &[(&str, &str, &str, &str)] = &[
    (
        "transient.origin_storage_key",
        "crates/dustnet-core/src/protocol/origin.rs",
        "Origin::storage_key",
        "result",
    ),
    (
        "transient.session_directive_serialize",
        "crates/dustnet-core/src/session.rs",
        "SessionDirective::serialize",
        "result",
    ),
    (
        "transient.session_directive_parse_set",
        "crates/dustnet-core/src/session.rs",
        "SessionDirective::parse_set",
        "result",
    ),
    (
        "transient.session_directive_parse_clear",
        "crates/dustnet-core/src/session.rs",
        "SessionDirective::parse_clear",
        "result",
    ),
    (
        "transient.protocol_diagnostic",
        "crates/dustnet-core/src/protocol/mod.rs",
        "try_protocol_diagnostic",
        "result",
    ),
    (
        "transient.transport_address",
        "crates/dustnet-client/src/transport.rs",
        "try_format_host_port",
        "result",
    ),
    (
        "transient.transport_server_name",
        "crates/dustnet-client/src/transport.rs",
        "try_server_name",
        "result",
    ),
    (
        "transient.scanner_sanitized",
        "crates/dustnet-core/src/scanner/mod.rs",
        "Scanner::new",
        "scratch",
    ),
    (
        "transient.scanner_tokens",
        "crates/dustnet-core/src/scanner/mod.rs",
        "Scanner::scan_all",
        "result",
    ),
    (
        "transient.component_expansion",
        "crates/dustnet-core/src/parser/components.rs",
        "expand_components",
        "result",
    ),
    (
        "transient.parser_result",
        "crates/dustnet-core/src/parser/mod.rs",
        "parse",
        "result",
    ),
    (
        "transient.parser_validation",
        "crates/dustnet-core/src/parser/validate.rs",
        "validate_triggers",
        "result",
    ),
    (
        "transient.local_wasm_read",
        "crates/dustnet-client/src/compositor/animate/wasm.rs",
        "load_wasm_from_file",
        "result",
    ),
    (
        "transient.history_entry_try_clone",
        "crates/dustnet-client/src/viewer.rs",
        "HistoryEntry::try_clone",
        "result",
    ),
    (
        "transient.wasm_dependency_paths",
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "wasm_dependency_paths",
        "result",
    ),
    (
        "transient.server_read_static_body",
        "crates/dustnet-server/src/lib.rs",
        "read_static_body",
        "result",
    ),
    (
        "transient.collect_form_values",
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "collect_form_values",
        "result",
    ),
    (
        "transient.url_encode_form",
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "url_encode_form",
        "result",
    ),
    (
        "transient.terminal_hud_frame",
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "draw_viewer_frame",
        "result",
    ),
    (
        "transient.protocol.encode_frame",
        "crates/dustnet-core/src/protocol/frame.rs",
        "encode_frame",
        "result",
    ),
    (
        "transient.protocol.allocate_frame_body",
        "crates/dustnet-core/src/protocol/frame.rs",
        "allocate_frame_body",
        "result",
    ),
    (
        "transient.protocol.page_encode",
        "crates/dustnet-core/src/protocol/message.rs",
        "PageMessage::encode_body",
        "result",
    ),
    (
        "transient.protocol.page_decode",
        "crates/dustnet-core/src/protocol/message.rs",
        "PageMessage::decode_body",
        "result",
    ),
    (
        "transient.protocol.hello_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "HelloMessage::serialize",
        "result",
    ),
    (
        "transient.protocol.welcome_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "WelcomeMessage::serialize",
        "result",
    ),
    (
        "transient.protocol.get_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "GetMessage::serialize",
        "result",
    ),
    (
        "transient.protocol.redirect_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "RedirectMessage::serialize",
        "result",
    ),
    (
        "transient.protocol.error_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "ErrorMessage::serialize",
        "result",
    ),
    (
        "transient.protocol.input_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "InputMessage::serialize",
        "result",
    ),
    (
        "transient.protocol.subscribe_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "SubscribeMessage::serialize",
        "result",
    ),
    (
        "transient.protocol.update_serialize",
        "crates/dustnet-core/src/protocol/message.rs",
        "UpdateMessage::serialize",
        "result",
    ),
];

/// Structs that own heap storage but are deliberately outside the governed
/// inventory. Every entry carries a reason. The checker rejects an entry whose
/// struct no longer exists or no longer owns heap storage, so this list cannot
/// silently rot into a blanket waiver.
const EXEMPT: &[(&str, &str, &str)] = &[
    (
        "crates/dustnet/src/main.rs",
        "ConnectOpts",
        "local CLI arguments; not remotely influenced",
    ),
    (
        "crates/dustnetd/src/main.rs",
        "Cli",
        "local CLI arguments; parsed once at startup and never remotely influenced",
    ),
    (
        "crates/dustnet-client/src/client.rs",
        "ActiveSubscription",
        "slot of AtpClient.active_subscriptions (row client.active_subscriptions), whose bound and admission cover the endpoint/region payload leases held here",
    ),
    (
        "crates/dustnet-client/src/client.rs",
        "PendingUpdate",
        "slot of AtpClient.pending_updates (row client.pending_updates), whose fixed ring admits the update, owner clone and lease together",
    ),
    (
        "crates/dustnet-client/src/client.rs",
        "FetchResult",
        "operation-lifetime move-only transfer of protocol-owned AML, URI and scope; the String is accounted by row transient.protocol.page_decode",
    ),
    (
        "crates/dustnet-client/src/client.rs",
        "ScopedRedirect",
        "operation-lifetime transfer value; scope and target move through the reducer's exact-owner slots without cloning",
    ),
    (
        "crates/dustnet-client/src/client.rs",
        "ScopedUpdate",
        "operation-lifetime transfer value; the lease and scope it carries belong to row client.pending_updates",
    ),
    (
        "crates/dustnet-client/src/client.rs",
        "TlsPolicy",
        "certificate authorities read once at startup from a local PEM file named on the command line; never remotely influenced",
    ),
    (
        "crates/dustnet-client/src/transport.rs",
        "CaVerifier",
        "a shared reference to rustls's own verifier plus one cell holding the fingerprint and stated reason of a certificate that failed; allocated per connection attempt and dropped with it",
    ),
    (
        "crates/dustnet-client/src/transport.rs",
        "UnverifiedPeer",
        "one fixed-size fingerprint and rustls's own account of a refusal, moved out of a failed handshake to be shown once",
    ),
    (
        "crates/dustnet-client/src/transport.rs",
        "PinningVerifier",
        "one fixed-size fingerprint cell and a shared reference to the process crypto provider, allocated per connection attempt and dropped with it; nothing here is remotely sized",
    ),
    (
        "crates/dustnet-client/src/trust.rs",
        "TrustStore",
        "one fixed-size pin per host and port the user connected to under --tofu, read from and written to a local file. A hostile server can redirect across hosts and so add entries, but each is ~110 bytes and requires a connection this client made; a cap is deliberately absent because evicting a pin silently reopens the first-use window it exists to close",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/mod.rs",
        "AnimationResizeCandidate",
        "animation tick/resize transfer value; its collections are pre-admitted with an exact _collection_lease before any adapter state advances",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/mod.rs",
        "AdvanceResult",
        "animation tick/resize transfer value; its collections are pre-admitted with an exact _collection_lease before any adapter state advances",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/runtime.rs",
        "AnimationNodeSnapshot",
        "animation tick/resize transfer value; its collections are pre-admitted with an exact _collection_lease before any adapter state advances",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/runtime.rs",
        "OutputStorage",
        "animation tick/resize transfer value; its collections are pre-admitted with an exact _collection_lease before any adapter state advances",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/runtime.rs",
        "PreparedAnimationResize",
        "animation tick/resize transfer value; its collections are pre-admitted with an exact _collection_lease before any adapter state advances",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/runtime.rs",
        "TickResult",
        "animation tick/resize transfer value; its collections are pre-admitted with an exact _collection_lease before any adapter state advances",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/runtime.rs",
        "SkipResult",
        "animation tick/resize transfer value; its collections are pre-admitted with an exact _collection_lease before any adapter state advances",
    ),
    (
        "crates/dustnet-client/src/compositor/animate/wasm.rs",
        "NoticeWriter",
        "bounded fallible writer; the String it builds is accounted by the calling site's row, never retained here",
    ),
    (
        "crates/dustnet-client/src/compositor/composite.rs",
        "PresentationCheckpoint",
        "compositor rollback checkpoint; holds inline bounding rectangles and a moved buffer already accounted by its scene row",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/cell.rs",
        "CellBuffer",
        "cells is the governed SceneCells/CompositorCells storage; every CellBuffer is admitted by the row that leases it (scene, live-region, overlay, canvas and snapshot rows)",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/cell.rs",
        "GraphemeStorage",
        "shared multi-scalar grapheme payload; carries its own RemoteCollections lease, accounted at the owning CellBuffer",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/cell.rs",
        "Cell",
        "one grapheme slot inside a governed CellBuffer",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "GovernedTempVec",
        "governed layout transient; carries its own exact lease and is released after rendering",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "PlacedElement",
        "loaded-page projection element; its AML-id and live-endpoint capacity is pre-admitted and retained by the LoadedPage projection rows",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "LayoutResult",
        "layout transfer value; the buffer, placed, focusable and sticky capacities it moves are the governed projection leases it carries",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "InlineSegment",
        "layout transient; capacity is pre-admitted and released with the enclosing governed temporary (GovernedTempVec / InlineLines / InlineSegments)",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "InlineSpan",
        "layout transient; capacity is pre-admitted and released with the enclosing governed temporary (GovernedTempVec / InlineLines / InlineSegments)",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "InlineLine",
        "layout transient; capacity is pre-admitted and released with the enclosing governed temporary (GovernedTempVec / InlineLines / InlineSegments)",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "InlineLines",
        "governed layout transient; carries its own exact lease and is released after rendering",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/text.rs",
        "WrappedLines",
        "governed layout transient; the plain-text counterpart of InlineLines, carrying its own lease reconciled to the measured capacity and released after rendering",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "StyledWord",
        "layout transient; capacity is pre-admitted and released with the enclosing governed temporary (GovernedTempVec / InlineLines / InlineSegments)",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/kinds/mod.rs",
        "InlineSegments",
        "governed layout transient; carries its own exact lease and is released after rendering",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/kinds/table.rs",
        "TableCellData",
        "layout transient; capacity is pre-admitted and released with the enclosing governed temporary (GovernedTempVec / InlineLines / InlineSegments)",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/text.rs",
        "WrappedLine",
        "layout transient; capacity is pre-admitted and released with the enclosing governed temporary (GovernedTempVec / InlineLines / InlineSegments)",
    ),
    (
        "crates/dustnet-client/src/compositor/panels.rs",
        "FocusableElement",
        "loaded-page focus projection element; AML id, label, action and toggle strings are pre-admitted and reconciled to exact retained capacity",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "FlowData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "AbsoluteData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "TextContent",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "TextRun",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "InputData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "SelectData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "OptionData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "ButtonData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "AnimationData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "LiveData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "LinkData",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/node.rs",
        "NodeBuilder",
        "NodeKind payload; string capacity is summed by Scene::retained_string_capacity and charged as AstStrings at page admission (rows scene.*)",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/patch.rs",
        "NodeTemplate",
        "patch construction cursor; the AML id it holds moves into the admitted Node",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/tree.rs",
        "NodeBufferCheckpoint",
        "scene rollback checkpoint; the buffer it holds is the governed Node.buffer storage being restored",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/tree.rs",
        "TreeOrderIter",
        "traversal cursor; its stack is bounded by current scene depth and released with the walk",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/events.rs",
        "DeferredNavigation",
        "page-lifetime deferred action; admitted with the candidate page in the fixed authored-action queue",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/presentation.rs",
        "FallibleLine",
        "bounded fallible writer; the String it builds is accounted by the calling site's row, never retained here",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/presentation.rs",
        "InputMode",
        "scoped projection of the reducer's input value; the String is the model's, re-projected per frame",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/rendering.rs",
        "FallibleString",
        "bounded fallible writer; the String it builds is accounted by the calling site's row, never retained here",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "PagePreparationRejected",
        "rejection carrier; holds the AstStrings lease being returned to the caller so it is released exactly once",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "HistoryEntry",
        "renderer history artifact; title capacity is charged by its _budget_lease and the logical entry lives in row history.*",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "PendingHistoryArtifact",
        "pending renderer history artifact; title capacity is charged by the budget_lease held here",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "DeferredProposal",
        "page-lifetime deferred action proposal; admitted with the candidate page in the fixed authored-action queue",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "ParsedPage",
        "operation-lifetime parsed candidate; the AML String moves without cloning and the parse lease is the row transient.parser_result admission",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "PendingPageTransition",
        "page-transition capture; the snapshot it holds is a governed CompositorCells buffer",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "FallibleFrame",
        "bounded fallible writer; the String it builds is accounted by the calling site's row, never retained here",
    ),
    (
        "crates/dustnet-client/src/compositor/terminal/runner.rs",
        "LayoutOutput",
        "layout transfer value; the buffer, placed and sticky capacities it moves are the governed projection leases it carries",
    ),
    (
        "crates/dustnet-client/src/compositor/wasm.rs",
        "HostState",
        "WASM host call state; content and output buffers are charged through the guest's Wasm lease held in the same struct",
    ),
    (
        "crates/dustnet-client/src/config.rs",
        "StatusBarConfig",
        "local client configuration; not remotely influenced",
    ),
    (
        "crates/dustnet-client/src/config.rs",
        "StatusBarVars",
        "local status-bar render variables; not remotely influenced",
    ),
    (
        "crates/dustnet-client/src/resource.rs",
        "ResourceGovernor",
        "the governor's own fixed-size accounting state; it measures remote memory rather than holding it",
    ),
    (
        "crates/dustnet-client/src/viewer.rs",
        "PageScope",
        "opaque reducer-issued identity; its only storage is a bounded Origin, whose host derives from a URI capped at MAX_URI_LEN",
    ),
    (
        "crates/dustnet-client/src/viewer.rs",
        "OperationOwner",
        "opaque reducer-issued identity; its only storage is a bounded Origin, whose host derives from a URI capped at MAX_URI_LEN",
    ),
    (
        "crates/dustnet-client/src/viewer.rs",
        "ControlToken",
        "opaque reducer-issued identity; its only storage is a bounded Origin, whose host derives from a URI capped at MAX_URI_LEN",
    ),
    (
        "crates/dustnet-client/src/viewer.rs",
        "PendingResizeProjection",
        "reducer-issued resize projection; holds only an opaque owner identity",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "Page",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "MetaEntry",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "BoxElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "RowElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "ColElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "ContainerElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "NavElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "TextElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "PreElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "HeadingElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "ListElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "ItemElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "LinkElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "InputElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "SelectElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "OptionElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "ButtonElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "FormElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "PanelElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "StateElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "DetailsElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "TriggerRef",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "OnElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "ArtElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "TableElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "TrElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "CellElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "AnimateElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "FrameElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "ElementDefElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "TweenElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "TextAnimateElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "LiveElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/ast.rs",
        "IncludeElement",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/mod.rs",
        "Diagnostic",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/mod.rs",
        "ParseResult",
        "AML AST payload; aggregate bounded by MAX_ELEMENTS, MAX_DEPTH and the scanner payload limits, constructed fallibly under row transient.parser_result",
    ),
    (
        "crates/dustnet-core/src/parser/mod.rs",
        "FallibleString",
        "bounded fallible writer; the String it builds is accounted by the calling site's row, never retained here",
    ),
    (
        "crates/dustnet-core/src/protocol/mod.rs",
        "ProtocolDiagnosticWriter",
        "bounded fallible writer; the String it builds is accounted by the calling site's row, never retained here",
    ),
    (
        "crates/dustnet-core/src/protocol/origin.rs",
        "Origin",
        "canonical origin; host derives from a URI capped at MAX_URI_LEN and is built with try_reserve_exact",
    ),
    (
        "crates/dustnet-core/src/scanner/mod.rs",
        "Attribute",
        "scanner token payload; bounded by rows scanner.chars and transient.scanner_tokens",
    ),
    (
        "crates/dustnet-core/src/session.rs",
        "SessionToken",
        "session directive value; bounded by the session rows core.session.sites and core.session.tokens",
    ),
    (
        "crates/dustnet-server/src/lib.rs",
        "SubscriptionBudget",
        "server-instance budget handle; fixed-size accounting state shared by row server.subscription_*",
    ),
    (
        "crates/dustnet-server/src/lib.rs",
        "StaticServerConfig",
        "local operator configuration; the root path is an operator-supplied regular directory",
    ),
    (
        "crates/dustnet-server/src/live_watch.rs",
        "AttachedWatch",
        "attach transfer value; the generation it carries is refcounted storage already charged by row watch.generation_text",
    ),
    (
        "crates/dustnet-server/src/live_watch.rs",
        "NotifyChangeSource",
        "watch-descriptor refcounts keyed by directory; bounded by the watched-file ceiling enforced in row watch.files",
    ),
];

/// Module isolation rules, checked over the crate-internal `use` graph rather
/// than by searching source text for magic strings. A rename cannot defeat
/// these, and a mention in a comment cannot trip them: reaching a forbidden
/// module requires importing it, directly or through something it imports.
const MODULE_ISOLATION: &[(&str, &[&str], &str)] = &[(
    "crates/dustnet-client/src/compositor/animate",
    &[
        "crate::client",
        "crate::transport",
        "crate::viewer",
        "crate::session_store",
    ],
    "animation construction and ticking must not reach the network or the \
     viewer reducer; remote WASM arrives only as reducer-prepared bytes",
)];

/// Modules whose **function-local** collection allocation is enumerated by
/// [`scan_local_allocations`]. Struct discovery cannot see a `Vec` that lives
/// in a function body, so the registry being closed says nothing about these.
/// This is where the compositor builds scenes and lays out pages directly from
/// remote content, which is where an unbounded local accumulation would matter.
const LOCAL_ALLOCATION_ROOTS: &[&str] = &[
    "crates/dustnet-client/src/compositor/scene/build.rs",
    "crates/dustnet-client/src/compositor/layout/",
];

/// Expressions that construct a fresh growable collection.
///
/// The list is deliberately narrow, and its scope is stated rather than
/// implied: it covers the accumulate-into-a-new-collection shapes named in
/// the function-local collection backlog and nothing else. `.clone()`, `.to_string()`, `.to_owned()`
/// and `format!` are excluded on purpose — each copies a value whose size was
/// already admitted where that value was first allocated, rather than growing
/// a new collection from a loop over remote content.
const COLLECTION_CONSTRUCTORS: &[&str] = &[
    "Vec::new(",
    "Vec::with_capacity(",
    "VecDeque::new(",
    "VecDeque::with_capacity(",
    "String::new(",
    "String::with_capacity(",
    "BTreeMap::new(",
    "BTreeSet::new(",
    "HashMap::new(",
    "HashSet::new(",
    "vec![",
    ".collect(",
    ".collect::",
    ".to_vec(",
];

/// Discovered local sites that are not conversion backlog, each with a written
/// reason. This mirrors `EXEMPT`: the scan stays deliberately literal, and any
/// claim that a site does not need pre-reservation is stated here rather than
/// encoded as a heuristic the scan could apply silently to a site it should
/// not.
///
/// Keyed by `(source, function, constructor, binding)`, where `-` means the
/// constructor is not bound by a `let`. One key can cover several sites in the
/// same function; a key matching no site fails as stale.
const LOCAL_EXEMPT: &[(&str, &str, &str, &str, &str)] = &[
    (
        "crates/dustnet-client/src/compositor/layout/border.rs",
        "draw_border_with_joints",
        ".collect(",
        "truncated",
        "bounded by the drawn border width, which comes from the terminal size \
         rather than from remote content",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/text.rs",
        "blank_line",
        "String::new(",
        "-",
        "the empty string standing for a blank wrapped line; it is returned by \
         value and never grown, so it never allocates",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/kinds/input.rs",
        "layout",
        "String::new(",
        "visible",
        "bounded by the input field's content width, which is the terminal \
         width capped at 40 columns rather than remote content",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/kinds/flow.rs",
        "layout_list",
        "String::new(",
        "-",
        "the empty marker for `ListStyle::None`; it is compared against and \
         never grown",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/kinds/table.rs",
        "layout",
        "String::new(",
        "s",
        "the ellipsis-truncated cell label, bounded by the column width, \
         which derives from the terminal width rather than remote content",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "empty_inline_line",
        "Vec::new(",
        "-",
        "the empty span vector standing for a blank inline line; it is \
         returned by value and never grown, so it never allocates",
    ),
    (
        "crates/dustnet-client/src/compositor/layout/engine.rs",
        "empty_layout_metadata",
        "Vec::new(",
        "-",
        "the empty metadata triple returned when nothing is placed or the \
         admission is refused; none of the three is ever grown",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/build.rs",
        "build_text",
        "String::new(",
        "-",
        "the empty text of the style template run; every produced run \
         overrides it with `text:`, so the template's own string is never \
         grown",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/build.rs",
        "flatten_inline_text",
        "String::new(",
        "-",
        "the empty text of the merged style template; as in `build_text`, \
         each produced run overrides it and the template is never grown",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/build.rs",
        "from_document_with_governor",
        "Vec::new(",
        "-",
        "releases relation storage on rollback by assigning an empty vector; \
         `Vec::new` does not allocate and nothing is pushed after it",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/build.rs",
        "build_element_inner",
        "vec![",
        "-",
        "fixed one-element run vectors whose arity is a literal; the remote \
         content is the element already admitted into the run",
    ),
    (
        "crates/dustnet-client/src/compositor/scene/build.rs",
        "build_animation",
        "Vec::new(",
        "-",
        "empty initialiser for the governed `FrameAnimationAdapter.frames` \
         row; frames are admitted where they are pushed",
    ),
];

/// Fallible growth helpers: `(source, function, parameter)`.
///
/// A collection passed by `&mut` to one of these is reserved before every
/// push, so a site bound to it counts as pre-reserved even though no
/// `try_reserve` is spelled at the site itself. Without this the scan would
/// push design the wrong way — inlining a reservation at every call site
/// purely to satisfy the measurement is worse code than a helper that does it
/// once.
///
/// The claim is verified rather than trusted: the named function must exist in
/// the named file and its body must reserve the named parameter. A helper that
/// stops reserving fails the gate instead of quietly absolving its callers.
const LOCAL_FALLIBLE_HELPERS: &[(&str, &str, &str)] = &[(
    "crates/dustnet-client/src/compositor/layout/engine.rs",
    "try_push_span",
    "spans",
)];

const PROTOCOL_DISPATCH_PATHS: &[(&str, &str)] = &[
    ("crates/dustnet-client/src/transport.rs", "try_send_frame"),
    ("crates/dustnet-client/src/transport.rs", "try_recv_frame"),
    ("crates/dustnet-server/src/transport.rs", "try_send_frame"),
    ("crates/dustnet-server/src/transport.rs", "try_recv_frame"),
];

#[derive(Debug)]
struct Row {
    id: String,
    source: String,
    owner: String,
    field: String,
    status: String,
    tests: String,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg != "--check" && arg != "--report") {
        eprintln!("usage: dustnet-allocation-audit [--check] [--report]");
        std::process::exit(2);
    }
    if args.iter().any(|arg| arg == "--report") {
        report(&repository_root());
        return;
    }
    if let Err(errors) = check(&repository_root()) {
        for error in errors {
            eprintln!("allocation audit: {error}");
        }
        std::process::exit(1);
    }
}

/// Prints the discovered registry so unclassified heap owners can be triaged.
fn report(root: &Path) {
    let (heap_owners, unclassified, struct_count) = scan_registry(root);
    println!(
        "structs={struct_count} heap-owning={} registered={} exempt={} unclassified={}",
        heap_owners.len(),
        heap_owners.len() - EXEMPT.len() - unclassified.len(),
        EXEMPT.len(),
        unclassified.len()
    );
    for owner in &unclassified {
        println!(
            "{}\t{}\t{}",
            owner.source,
            owner.name,
            owner.fields.iter().cloned().collect::<Vec<_>>().join(",")
        );
    }
    let sites = scan_local_allocations(root);
    let (reserved, exempt, backlog) = local_allocation_counts(&sites);
    println!(
        "\nlocal-allocation sites={} reserved={reserved} exempt={exempt} backlog={backlog}",
        sites.len()
    );
    for site in sites.iter().filter(|site| site.is_backlog()) {
        println!(
            "{}:{}\t{}\t{}\t{}",
            site.source,
            site.line,
            site.function,
            site.constructor,
            site.binding.as_deref().unwrap_or("-")
        );
    }

    match read_inventory(&root.join("verification/allocation-owners.tsv")) {
        Ok(rows) => {
            let indexed: BTreeMap<_, _> = rows
                .into_iter()
                .map(|row| {
                    (
                        (row.source.clone(), row.owner.clone(), row.field.clone()),
                        row,
                    )
                })
                .collect();
            let coverage = hook_coverage(root, &indexed);
            let governed = indexed
                .values()
                .filter(|row| row.status == "governed")
                .count();
            println!(
                "\nrejection-hooks symbols={} governed={governed} hooked={} unhooked={}",
                coverage.symbols.len(),
                governed - coverage.bare.len(),
                coverage.bare.len()
            );
            for row in &coverage.bare {
                println!("{row}");
            }
        }
        Err(error) => println!("\nrejection-hooks unavailable: {error}"),
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tool must live at tools/allocation-audit")
        .to_path_buf()
}

/// One discovered production struct and whether it owns heap storage.
struct Discovered {
    source: String,
    name: String,
    fields: BTreeSet<String>,
}

/// Discovers every production struct and partitions the heap-owning ones into
/// registered authorities, declared exemptions, and unclassified owners.
fn scan_registry(root: &Path) -> (Vec<Discovered>, Vec<Discovered>, usize) {
    let authorities: BTreeSet<_> = AUTHORITIES.iter().copied().collect();
    let exempt: BTreeSet<_> = EXEMPT
        .iter()
        .map(|(source, name, _)| (*source, *name))
        .collect();
    let mut heap_owners = Vec::new();
    let mut unclassified = Vec::new();
    let mut total = 0usize;
    for (source, text) in production_sources(root) {
        let production = strip_cfg_test(&text);
        for name in declared_structs(&production) {
            total += 1;
            let Ok(fields) = heap_fields(&production, &name) else {
                continue;
            };
            if fields.is_empty() {
                continue;
            }
            let discovered = Discovered {
                source: source.clone(),
                name: name.clone(),
                fields,
            };
            let key = (source.as_str(), name.as_str());
            if !authorities.contains(&key) && !exempt.contains(&key) {
                unclassified.push(Discovered {
                    source: discovered.source.clone(),
                    name: discovered.name.clone(),
                    fields: discovered.fields.clone(),
                });
            }
            heap_owners.push(discovered);
        }
    }
    (heap_owners, unclassified, total)
}

fn check(root: &Path) -> Result<(), Vec<String>> {
    let rows =
        read_inventory(&root.join("verification/allocation-owners.tsv")).map_err(|e| vec![e])?;
    let test_sources = rust_source_corpus(&root.join("crates"));
    let mut errors = Vec::new();
    let mut ids = BTreeSet::new();
    let mut indexed = BTreeMap::new();
    for row in rows {
        if !ids.insert(row.id.clone()) {
            errors.push(format!("duplicate id `{}`", row.id));
        }
        let key = (row.source.clone(), row.owner.clone(), row.field.clone());
        if indexed.insert(key.clone(), row).is_some() {
            errors.push(format!(
                "duplicate field row `{}::{}.{}`",
                key.0, key.1, key.2
            ));
        }
    }

    let authorities: BTreeSet<_> = AUTHORITIES.iter().copied().collect();
    let transient_authorities: BTreeSet<_> = TRANSIENT_SITES
        .iter()
        .map(|(_, source, owner, _)| (*source, *owner))
        .collect();
    for ((source, owner, field), row) in &indexed {
        if !authorities.contains(&(source.as_str(), owner.as_str()))
            && !transient_authorities.contains(&(source.as_str(), owner.as_str()))
        {
            errors.push(format!(
                "row `{}` names untracked authority `{source}::{owner}`",
                row.id
            ));
        }
        if !matches!(
            row.status.as_str(),
            "governed" | "bounded" | "gap" | "local"
        ) {
            errors.push(format!(
                "row `{}` has invalid status `{}`",
                row.id, row.status
            ));
        }
        if row.status == "governed" && row.tests == "none" {
            errors.push(format!(
                "governed row `{}` must name accounting tests",
                row.id
            ));
        }
        if row.status == "governed" {
            for test in named_tests(&row.tests) {
                if !test_sources.contains(&format!("fn {test}")) {
                    errors.push(format!(
                        "governed row `{}` names missing test `{test}`",
                        row.id
                    ));
                }
            }
        }
        if field.is_empty() {
            errors.push(format!("row `{}` has an empty field", row.id));
        }
    }

    for &(source, owner) in AUTHORITIES {
        let path = root.join(source);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                errors.push(format!("read {}: {error}", path.display()));
                continue;
            }
        };
        let fields = match heap_fields(&text, owner) {
            Ok(fields) => fields,
            Err(error) => {
                errors.push(format!("{source}: {error}"));
                continue;
            }
        };
        for field in &fields {
            let key = (source.to_string(), owner.to_string(), field.clone());
            if !indexed.contains_key(&key) {
                errors.push(format!("missing row for `{source}::{owner}.{field}`"));
            }
        }
        for ((row_source, row_owner, field), row) in &indexed {
            if row_source == source && row_owner == owner && !fields.contains(field) {
                errors.push(format!("stale row `{}` for absent field `{field}`", row.id));
            }
        }
    }

    for &(id, source, owner, field) in TRANSIENT_SITES {
        let key = (source.to_string(), owner.to_string(), field.to_string());
        match indexed.get(&key) {
            Some(row) if row.id == id => {}
            Some(row) => errors.push(format!(
                "transient site `{source}::{owner}` uses row `{}` instead of `{id}`",
                row.id
            )),
            None => errors.push(format!("missing row for transient site `{source}::{owner}")),
        }
        let path = root.join(source);
        match fs::read_to_string(&path) {
            Ok(text) if function_exists(&text, owner) => {}
            Ok(_) => errors.push(format!("transient site `{source}::{owner}` is stale")),
            Err(error) => errors.push(format!("read {}: {error}", path.display())),
        }
    }

    let (heap_owners, unclassified, _) = scan_registry(root);
    for owner in &unclassified {
        errors.push(format!(
            "unclassified heap owner `{}::{}` (fields: {}) - add an inventory row or an EXEMPT entry",
            owner.source,
            owner.name,
            owner.fields.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    let live: BTreeSet<_> = heap_owners
        .iter()
        .map(|owner| (owner.source.as_str(), owner.name.as_str()))
        .collect();
    for &(source, name) in AUTHORITIES {
        if !live.contains(&(source, name)) {
            errors.push(format!(
                "stale authority `{source}::{name}`: absent, or no longer owns heap storage"
            ));
        }
    }
    for &(source, name, reason) in EXEMPT {
        if reason.is_empty() {
            errors.push(format!("exemption `{source}::{name}` has no reason"));
        }
        if !live.contains(&(source, name)) {
            errors.push(format!(
                "stale exemption `{source}::{name}`: absent, or no longer owns heap storage"
            ));
        }
    }

    let local_sites = scan_local_allocations(root);
    for (source, function, parameter) in LOCAL_FALLIBLE_HELPERS {
        let path = root.join(source);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let code = mask_code(&text);
                if helper_reserves_parameter(&code, function).is_none() {
                    errors.push(format!(
                        "fallible helper `{source}::{function}` does not reserve `{parameter}`; \
                         sites relying on it are no longer pre-reserved"
                    ));
                }
            }
            Err(error) => errors.push(format!("read {}: {error}", path.display())),
        }
    }
    for (source, function, constructor, binding, reason) in LOCAL_EXEMPT {
        if reason.trim().is_empty() {
            errors.push(format!(
                "local exemption `{source}::{function}` `{constructor}` has no reason"
            ));
        }
        let matched = local_sites.iter().any(|site| {
            site.source == *source
                && site.function == *function
                && site.constructor == *constructor
                && site.binding.as_deref().unwrap_or("-") == *binding
        });
        if !matched {
            errors.push(format!(
                "stale local exemption `{source}::{function}` `{constructor}` bound to \
                 `{binding}`: no such site"
            ));
        }
    }
    // The injection state must not exist in a release build. The hardening
    // record asserts this — "compiled away entirely in release builds" — and
    // nothing checked it. A hook whose thread-local survives `#[cfg(test)]` stripping
    // is reachable from production code, which is a different and much worse
    // thing than a test-only hook.
    for (source, text) in production_sources(root) {
        let production = strip_cfg_test(&text);
        for symbol in rejection_hooks(&text) {
            if symbol
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch == '_')
                && mentions_token(&production, &symbol)
            {
                errors.push(format!(
                    "rejection hook state `{source}::{symbol}` survives `#[cfg(test)]` \
                     stripping; it would exist in a release build"
                ));
            }
        }
    }

    let coverage = hook_coverage(root, &indexed);
    // §3's closure condition, enforced rather than reported: a governed row
    // whose named tests never drive a rejection hook is a row whose refusal
    // path nothing exercises. This is a hard failure now that the count is
    // zero — the only way it can come back is a new governed row or a test
    // that stopped using its hook.
    for row in &coverage.bare {
        errors.push(format!(
            "governed row `{row}` names no test that drives an owner-keyed rejection hook"
        ));
    }

    // A version string without a pre-release suffix claims something shipped.
    // It does not ship without the gate behind it being written down.

    // Fuzzing depth as an artifact rather than a memory: every target must
    // have been run against this exact version, with the numbers recorded.
    for target in uncampaigned_fuzz_targets(root) {
        errors.push(format!(
            "fuzz target `{target}` has no row in {FUZZ_LOG} for the current \
             workspace version; run `make fuzz-campaign`"
        ));
    }

    // Panic-freedom, checked rather than described: a crate root that loses
    // its deny block silently reopens the whole class, and a bare `#[allow]`
    // of one of these lints is an exemption with no argument attached.
    let panic_gate = panic_gate_status(root);
    for (name, gated, unreasoned) in &panic_gate {
        if !gated {
            errors.push(format!(
                "crate `{name}` does not deny the panic lints at its root; remote input \
                 can reach a panicking operation there without failing the build"
            ));
        }
        if *unreasoned > 0 {
            errors.push(format!(
                "crate `{name}` has {unreasoned} `#[allow]` of a panic lint without \
                 `reason = \"...\"`; an exemption must carry its argument"
            ));
        }
    }

    // Precondition for claiming a platform the full gate did not run on: no
    // crate source or test is excluded from compilation by platform, so a
    // macOS gate run type-checks the Linux paths too. The platform claims
    // themselves are in docs/guides/production-support.md.
    for item in platform_gated_items(root) {
        errors.push(format!(
            "`{item}` gates an item on `target_os`, so one platform never \
             compiles it; select the branch with a `cfg!` expression, or \
             skip the run with `#[cfg_attr(target_os = ..., ignore = \"...\")]`"
        ));
    }

    for test in unmarked_async_tests(root) {
        errors.push(format!(
            "`{test}` builds a tokio runtime under a Miri-gated root without \
             `#[cfg_attr(miri, ignore)]`; it would abort the whole Miri run"
        ));
    }

    // Release gate: every adversary the threat model names maps to a control
    // and to evidence that exists. The model is data, not prose — nesting
    // coverage inside each adversary means a row cannot name an adversary
    // that is not listed, so that failure mode is gone rather than checked.
    let makefile = fs::read_to_string(root.join("Makefile")).unwrap_or_default();
    match threat_model(root) {
        Err(error) => errors.push(error),
        Ok(model) => {
            if model.adversaries.is_empty() {
                errors.push(format!(
                    "{THREAT_MODEL} lists no adversaries; threat coverage cannot be checked"
                ));
            }
            for adversary in &model.adversaries {
                if adversary.coverage.is_empty() {
                    errors.push(format!(
                        "threat model names adversary `{}` with no control or evidence",
                        adversary.name
                    ));
                }
                for row in &adversary.coverage {
                    if !model.properties.contains(&row.property) {
                        errors.push(format!(
                            "threat coverage for `{}` names property `{}`, which is not \
                             one of the declared properties",
                            adversary.name, row.property
                        ));
                    }
                    let is_test = test_sources.contains(&format!("fn {}", row.evidence));
                    let is_target = makefile.contains(&format!("\n{}:", row.evidence));
                    if !is_test && !is_target {
                        errors.push(format!(
                            "threat coverage for `{}`/`{}` cites evidence `{}`, which is \
                             neither a test nor a make target",
                            adversary.name, row.property, row.evidence
                        ));
                    }
                }
            }
        }
    }

    // §4: every architectural claim carries its enforcement class, and no
    // claim called structural is implemented by searching source text.
    let searchers = source_searching_tests(root);
    let declared_lints: BTreeSet<&str> = LINT_ENFORCED.iter().map(|(_, test)| *test).collect();
    for test in &searchers {
        if !declared_lints.contains(test.as_str()) {
            errors.push(format!(
                "`{test}` searches source text but is not declared in LINT_ENFORCED; \
                 a claim backed by a substring assertion must say so"
            ));
        }
    }
    for (claim, test) in LINT_ENFORCED {
        if !searchers.contains(*test) {
            errors.push(format!(
                "lint-enforced claim \"{claim}\" names `{test}`, which no longer searches \
                 source text: either the test is gone or the claim is now structural"
            ));
        }
    }
    for (claim, path, token) in STRUCTURAL_ENFORCED {
        if LINT_ENFORCED.iter().any(|(lint, _)| lint == claim) {
            errors.push(format!(
                "claim \"{claim}\" is declared both structural and lint-enforced"
            ));
            continue;
        }
        match fs::read_to_string(root.join(path)) {
            Ok(text) if text.contains(token) => {}
            Ok(_) => errors.push(format!(
                "structural claim \"{claim}\" cites `{path}` for `{token}`, which is absent"
            )),
            Err(error) => errors.push(format!("read {path}: {error}")),
        }
    }

    let mut status_counts = BTreeMap::<&str, usize>::new();
    for row in indexed.values() {
        *status_counts.entry(row.status.as_str()).or_default() += 1;
    }

    check_module_isolation(root, &mut errors);

    for &(source, function) in PROTOCOL_DISPATCH_PATHS {
        let path = root.join(source);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                errors.push(format!("read {}: {error}", path.display()));
                continue;
            }
        };
        let Some(body) = function_body(&text, function) else {
            errors.push(format!(
                "protocol dispatch path `{source}::{function}` is stale"
            ));
            continue;
        };
        let mut previous = 0usize;
        for required in [
            "validate_inbound_body_len(",
            "validate_inbound_flags(",
            "validate_frame_body(",
            "validate_capability(",
            ".state.apply(",
        ] {
            let Some(offset) = body[previous..]
                .find(required)
                .map(|found| previous + found)
            else {
                errors.push(format!(
                    "protocol dispatch path `{source}::{function}` bypasses `{required}`"
                ));
                continue;
            };
            let line_end = body[offset..]
                .find('\n')
                .map_or(body.len(), |end| offset + end);
            if !body[offset..line_end].trim_end().ends_with("?;") {
                errors.push(format!(
                    "protocol dispatch path `{source}::{function}` does not propagate `{required}`"
                ));
            }
            previous = offset + required.len();
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn read_inventory(path: &Path) -> Result<Vec<Row>, String> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(format!("{} has an invalid header", path.display()));
    }
    let mut rows = Vec::new();
    for (index, line) in lines.enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<_> = line.split('\t').collect();
        if columns.len() != COLUMN_COUNT {
            return Err(format!(
                "{}:{} expected {COLUMN_COUNT} columns, found {}",
                path.display(),
                index + 2,
                columns.len()
            ));
        }
        if columns.iter().any(|column| column.is_empty()) {
            return Err(format!(
                "{}:{} contains an empty column",
                path.display(),
                index + 2
            ));
        }
        if !matches!(columns[4], "remote" | "mixed" | "local") {
            return Err(format!(
                "{}:{} has invalid influence",
                path.display(),
                index + 2
            ));
        }
        if !matches!(
            columns[5],
            "operation" | "frame" | "page" | "connection" | "viewer" | "process"
        ) {
            return Err(format!(
                "{}:{} has invalid lifetime",
                path.display(),
                index + 2
            ));
        }
        if !matches!(columns[6], "retained" | "transient") {
            return Err(format!(
                "{}:{} has invalid storage",
                path.display(),
                index + 2
            ));
        }
        if columns[12] == "gap" && !columns[9].starts_with("missing") {
            return Err(format!(
                "{}:{} gap admission must start with `missing`",
                path.display(),
                index + 2
            ));
        }
        rows.push(Row {
            id: columns[0].to_string(),
            source: columns[1].to_string(),
            owner: columns[2].to_string(),
            field: columns[3].to_string(),
            status: columns[12].to_string(),
            tests: columns[13].to_string(),
        });
    }
    Ok(rows)
}

fn rust_source_corpus(root: &Path) -> String {
    fn visit(path: &Path, output: &mut String) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, output);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
                && let Ok(text) = fs::read_to_string(path)
            {
                output.push_str(&text);
                output.push('\n');
            }
        }
    }
    let mut output = String::new();
    visit(root, &mut output);
    output
}

fn heap_fields(source: &str, owner: &str) -> Result<BTreeSet<String>, String> {
    let tokens = lex(source);
    let Some(struct_index) = tokens.windows(2).position(|pair| pair == ["struct", owner]) else {
        return Err(format!("struct `{owner}` not found"));
    };
    // Classify the declaration form before reading fields. Scanning forward for
    // the next `{` is wrong for unit and tuple structs: it walks into whatever
    // item follows and attributes that item's fields to this one.
    let mut cursor = struct_index + 2;
    let mut generics = 0isize;
    let open = loop {
        let Some(token) = tokens.get(cursor) else {
            return Err(format!("struct `{owner}` has no body"));
        };
        match token.as_str() {
            "<" => generics += 1,
            ">" => generics -= 1,
            ";" if generics == 0 => return Ok(BTreeSet::new()),
            "(" if generics == 0 => {
                // Tuple struct: one positional field per heap-typed element.
                let mut fields = BTreeSet::new();
                let mut depth = 1isize;
                let mut position = 0usize;
                let mut element = Vec::new();
                cursor += 1;
                while let Some(token) = tokens.get(cursor) {
                    match token.as_str() {
                        "(" | "<" | "[" => depth += 1,
                        ")" | ">" | "]" => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        "," if depth == 1 => {
                            if is_heap_type(&element) {
                                fields.insert(position.to_string());
                            }
                            position += 1;
                            element.clear();
                            cursor += 1;
                            continue;
                        }
                        _ => {}
                    }
                    element.push(token.clone());
                    cursor += 1;
                }
                if is_heap_type(&element) {
                    fields.insert(position.to_string());
                }
                return Ok(fields);
            }
            "{" if generics == 0 => break cursor,
            _ => {}
        }
        cursor += 1;
    };
    let mut index = open + 1;
    let mut fields = BTreeSet::new();
    while index < tokens.len() && tokens[index] != "}" {
        while matches!(
            tokens.get(index).map(String::as_str),
            Some("pub" | "(" | ")" | "super" | "crate")
        ) {
            index += 1;
        }
        let Some(name) = tokens.get(index).cloned() else {
            break;
        };
        if tokens.get(index + 1).map(String::as_str) != Some(":") {
            index += 1;
            continue;
        }
        index += 2;
        let mut type_tokens = Vec::new();
        let mut nested = 0isize;
        while index < tokens.len() {
            let token = &tokens[index];
            if token == "," && nested == 0 {
                break;
            }
            if matches!(token.as_str(), "<" | "(" | "[" | "{") {
                nested += 1;
            }
            if matches!(token.as_str(), ">" | ")" | "]" | "}") {
                nested -= 1;
            }
            type_tokens.push(token.clone());
            index += 1;
        }
        if is_heap_type(&type_tokens) {
            fields.insert(name);
        }
        index += usize::from(tokens.get(index).map(String::as_str) == Some(","));
    }
    Ok(fields)
}

/// A borrowed field points at storage some other owner already accounts for.
fn is_borrowed(tokens: &[String]) -> bool {
    tokens.first().is_some_and(|token| token == "&")
}

fn is_heap_type(tokens: &[String]) -> bool {
    if is_borrowed(tokens) {
        return false;
    }
    const HEAP_TYPES: &[&str] = &[
        "Arc",
        "ActiveSubscriptions",
        "ArrayVec",
        "AmlIdIndex",
        "AnimationRuntime",
        "AtpUri",
        "AtpClient",
        "Box",
        "BudgetLease",
        "CellBuffer",
        "CommandLine",
        "Compositor",
        "DeferredNavigation",
        "DeferredProposal",
        "DirtyRegions",
        "ErrorLog",
        "EventDispatcher",
        "EventBindings",
        "FocusAction",
        "GovernedSessionStore",
        "HashMap",
        "HashSet",
        "HistoryEntries",
        "Invalidation",
        "LayoutInvalidation",
        "InputMode",
        "LoadedPage",
        "NavigationMetadata",
        "Origin",
        "OperationOwner",
        "PageScope",
        "PathBuf",
        "PendingHistoryArtifact",
        "PendingUpdates",
        "PreparedLayout",
        "PreparedSlot",
        "PreparedWasmArtifact",
        "PreparedWasmBatch",
        "ParsedPage",
        "PendingPageTransition",
        "RegionBuffer",
        "RegionBufferEntry",
        "RegionBuffers",
        "RelayoutJournal",
        "ResourceCache",
        "Rc",
        "SessionStore",
        "Scene",
        "SceneNodes",
        "ScopedResource",
        "SharedResource",
        "SharedFrame",
        "SlotMap",
        "String",
        "SubscriptionLease",
        "TickResult",
        "Vec",
        "VecDeque",
    ];
    tokens
        .iter()
        .any(|token| HEAP_TYPES.contains(&token.as_str()))
}

fn function_exists(source: &str, owner: &str) -> bool {
    if let Some((type_name, method)) = owner.split_once("::") {
        source.contains(&format!("impl {type_name}")) && source.contains(&format!("fn {method}("))
    } else {
        source.contains(&format!("fn {owner}("))
    }
}

fn function_body<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let signature = format!("fn {name}(");
    let start = source.find(&signature)?;
    let open = source[start..].find('{')? + start;
    let mut depth = 0usize;
    for (offset, byte) in source.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&source[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn lex(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index += 2;
            let mut depth = 1usize;
            while index + 1 < bytes.len() && depth > 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            continue;
        }
        if bytes[index] == b'"' {
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    index += 2;
                } else if bytes[index] == b'"' {
                    index += 1;
                    break;
                } else {
                    index += 1;
                }
            }
            continue;
        }
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(source[start..index].to_string());
            continue;
        }
        tokens.push((bytes[index] as char).to_string());
        index += 1;
    }
    tokens
}

/// Every production Rust source file, as `(repository-relative path, text)`.
/// Only `crates/*/src` is walked: integration tests under `crates/*/tests` are
/// not production allocation surface.
fn production_sources(root: &Path) -> Vec<(String, String)> {
    let mut output = all_crate_sources(root);
    let test_only = test_only_paths(&output);
    output.retain(|(path, _)| !test_only.contains(path));
    output
}

/// Every Rust source under `crates/*/src`, **including** files reached only
/// through a `#[cfg(test)] #[path = "..."] mod` declaration.
///
/// `production_sources` filters those out because they are not production
/// allocation surface. Checks about tests themselves need them back.
fn all_crate_sources(root: &Path) -> Vec<(String, String)> {
    fn visit(base: &Path, path: &Path, output: &mut Vec<(String, String)>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                visit(base, &path, output);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
                && let Ok(text) = fs::read_to_string(&path)
                && let Ok(relative) = path.strip_prefix(base)
            {
                output.push((relative.to_string_lossy().replace('\\', "/"), text));
            }
        }
    }
    let mut output = Vec::new();
    let crates = root.join("crates");
    let Ok(entries) = fs::read_dir(&crates) else {
        return output;
    };
    let mut members: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    members.sort();
    for member in members {
        let source_dir = member.join("src");
        if source_dir.is_dir() {
            visit(root, &source_dir, &mut output);
        }
    }
    output
}

/// Files pulled in only by a `#[cfg(test)] #[path = "..."] mod ...;`
/// declaration. They live under `src/` but are not production surface.
fn test_only_paths(sources: &[(String, String)]) -> BTreeSet<String> {
    const ATTRIBUTE: &str = "#[cfg(test)]";
    let mut excluded = BTreeSet::new();
    for (path, text) in sources {
        let directory = path.rsplit_once('/').map_or("", |(head, _)| head);
        let mut rest = text.as_str();
        while let Some(offset) = rest.find(ATTRIBUTE) {
            rest = &rest[offset + ATTRIBUTE.len()..];
            let window = &rest[..rest.len().min(200)];
            let Some(start) = window.find("#[path = \"") else {
                continue;
            };
            let tail = &window[start + "#[path = \"".len()..];
            let Some(end) = tail.find('"') else {
                continue;
            };
            excluded.insert(normalize_relative(directory, &tail[..end]));
        }
    }
    excluded
}

/// Resolves a `#[path]` value against the declaring file's directory.
fn normalize_relative(directory: &str, target: &str) -> String {
    let mut segments: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for segment in target.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Removes every `#[cfg(test)]` item so discovery sees production types only.
fn strip_cfg_test(source: &str) -> String {
    const ATTRIBUTE: &str = "#[cfg(test)]";
    let mut output = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(offset) = rest.find(ATTRIBUTE) {
        output.push_str(&rest[..offset]);
        let after = &rest[offset + ATTRIBUTE.len()..];
        let brace = after.find('{');
        let semicolon = after.find(';');
        let block_first = match (brace, semicolon) {
            (Some(brace), Some(semicolon)) => brace < semicolon,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if block_first {
            let brace = brace.expect("block_first implies a brace");
            let mut depth = 0usize;
            let mut end = after.len();
            for (index, byte) in after.as_bytes()[brace..].iter().enumerate() {
                match byte {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = brace + index + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            rest = &after[end..];
        } else if let Some(end) = semicolon {
            rest = &after[end + 1..];
        } else {
            return output;
        }
    }
    output.push_str(rest);
    output
}

/// Names every struct declared in a source file, in declaration order.
fn declared_structs(source: &str) -> Vec<String> {
    let tokens = lex(source);
    let mut names = Vec::new();
    for pair in tokens.windows(2) {
        if pair[0] == "struct"
            && pair[1]
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            && !names.contains(&pair[1])
        {
            names.push(pair[1].clone());
        }
    }
    names
}

/// The `crate::`-rooted module paths a source file imports.
///
/// Handles both `use crate::a::b::{C, D};` and the grouped form
/// `use crate::{a::B, c::D};`, single- or multi-line. A path that cannot be
/// read is not silently dropped: an unparsable `use crate::` is reported so a
/// new syntax cannot quietly open a hole in the isolation check.
fn crate_imports(source: &str) -> BTreeSet<String> {
    let mut imports = BTreeSet::new();
    let mut rest = source;
    while let Some(offset) = find_use_crate(rest) {
        let after = &rest[offset..];
        let statement = after.split(';').next().unwrap_or(after);
        if statement.trim_start().starts_with('{') {
            let group = statement.trim_start();
            let inner = group
                .find('{')
                .map(|start| &group[start + 1..])
                .unwrap_or_default();
            for item in split_top_level(inner) {
                let path = leading_path(item.trim());
                if !path.is_empty() {
                    imports.insert(format!("crate::{path}"));
                }
            }
        } else {
            let path = leading_path(statement);
            if !path.is_empty() {
                imports.insert(format!("crate::{path}"));
            }
        }
        rest = after;
        rest = &rest[1..];
    }
    imports
}

/// Offset just past the next `use crate::` / `pub use crate::` that starts a
/// statement rather than sitting inside a comment.
fn find_use_crate(source: &str) -> Option<usize> {
    const NEEDLE: &str = "use crate::";
    let mut searched = 0usize;
    while let Some(found) = source[searched..].find(NEEDLE) {
        let start = searched + found;
        let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
        let prefix = source[line_start..start].trim_start();
        let commented = prefix.starts_with("//") || prefix.starts_with('*');
        let attached = prefix
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !commented && !attached {
            return Some(start + NEEDLE.len());
        }
        searched = start + NEEDLE.len();
    }
    None
}

/// The leading `a::b::c` run of a path, with any trailing `::` removed.
fn leading_path(text: &str) -> String {
    let path: String = text
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
        .collect();
    path.trim_end_matches(':').to_string()
}

/// Splits a brace group's contents on commas at nesting depth zero.
fn split_top_level(inner: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0isize;
    let mut current = String::new();
    for character in inner.chars() {
        match character {
            '{' => depth += 1,
            '}' if depth == 0 => break,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                items.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    items.push(current);
    items
}

/// The submodules a file declares with `mod name;`. Import following alone is
/// not enough: `crate::compositor::scene` resolves to `scene.rs`, and anything
/// that file declares with `mod tree;` is reachable from it without ever
/// appearing in a `use crate::` path.
fn declared_submodules(path: &Path, source: &str) -> Vec<PathBuf> {
    let directory = match path.file_name().and_then(|name| name.to_str()) {
        Some("mod.rs" | "lib.rs" | "main.rs") => path.parent().map(Path::to_path_buf),
        _ => Some(path.with_extension("")),
    };
    let Some(directory) = directory else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for line in source.lines() {
        let line = line.trim_start();
        let rest = line
            .strip_prefix("mod ")
            .or_else(|| line.strip_prefix("pub mod "))
            .or_else(|| line.strip_prefix("pub(crate) mod "));
        let Some(rest) = rest else {
            continue;
        };
        let Some(name) = rest.split(';').next().map(str::trim) else {
            continue;
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let file = directory.join(format!("{name}.rs"));
        if file.is_file() {
            files.push(file);
        }
        let module = directory.join(name).join("mod.rs");
        if module.is_file() {
            files.push(module);
        }
    }
    files
}

/// Resolves a `crate::a::b` path to the files that could define it, longest
/// prefix first, so `crate::compositor::scene::Scene` finds `scene.rs` or
/// `scene/mod.rs` rather than missing because `Scene` is a type.
fn module_files(crate_src: &Path, path: &str) -> Vec<PathBuf> {
    let segments: Vec<&str> = path.trim_start_matches("crate::").split("::").collect();
    let mut candidates = Vec::new();
    for length in (1..=segments.len()).rev() {
        let mut base = crate_src.to_path_buf();
        for segment in &segments[..length] {
            base.push(segment);
        }
        let file = base.with_extension("rs");
        if file.is_file() {
            candidates.push(file);
        }
        let module = base.join("mod.rs");
        if module.is_file() {
            candidates.push(module);
        }
        if !candidates.is_empty() {
            break;
        }
    }
    candidates
}

/// Every file reachable from `roots` through crate-internal imports, with the
/// import path that first reached each one.
fn reachable_modules(crate_src: &Path, roots: &[PathBuf]) -> BTreeMap<String, Vec<String>> {
    let mut seen: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut queue: Vec<(PathBuf, Vec<String>)> = roots
        .iter()
        .map(|path| (path.clone(), Vec::new()))
        .collect();
    let mut found = BTreeMap::new();
    while let Some((path, trail)) = queue.pop() {
        if seen.contains_key(&path) {
            continue;
        }
        seen.insert(path.clone(), trail.clone());
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let production = strip_cfg_test(&text);
        for import in crate_imports(&production) {
            let mut next_trail = trail.clone();
            next_trail.push(import.clone());
            found
                .entry(import.clone())
                .or_insert_with(|| next_trail.clone());
            for file in module_files(crate_src, &import) {
                queue.push((file, next_trail.clone()));
            }
        }
        for file in declared_submodules(&path, &production) {
            queue.push((file, trail.clone()));
        }
    }
    found
}

/// Checks each module-isolation rule over the crate-internal import graph.
fn check_module_isolation(root: &Path, errors: &mut Vec<String>) {
    for &(module, forbidden, why) in MODULE_ISOLATION {
        let directory = root.join(module);
        if !directory.is_dir() {
            errors.push(format!("module isolation root `{module}` is stale"));
            continue;
        }
        // `crates/<name>/src` is the crate root for `crate::` paths.
        let Some(crate_src) = directory
            .ancestors()
            .find(|path| path.file_name().is_some_and(|name| name == "src"))
        else {
            errors.push(format!("cannot locate the crate root for `{module}`"));
            continue;
        };
        let mut roots = Vec::new();
        let Ok(entries) = fs::read_dir(&directory) else {
            errors.push(format!("module isolation root `{module}` is unreadable"));
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                roots.push(path);
            }
        }
        // The `foo.rs` beside a `foo/` directory declares the module itself.
        let sibling = directory.with_extension("rs");
        if sibling.is_file() {
            roots.push(sibling);
        }
        roots.sort();
        let reachable = reachable_modules(crate_src, &roots);
        for &banned in forbidden {
            if let Some(trail) = reachable
                .iter()
                .find(|(path, _)| *path == banned || path.starts_with(&format!("{banned}::")))
                .map(|(_, trail)| trail)
            {
                errors.push(format!(
                    "module isolation: `{module}` reaches `{banned}` via {} - {why}",
                    trail.join(" -> ")
                ));
            }
        }
    }
}

/// One function-local collection allocation discovered under
/// [`LOCAL_ALLOCATION_ROOTS`].
struct LocalSite {
    source: String,
    offset: usize,
    line: usize,
    function: String,
    constructor: String,
    binding: Option<String>,
    reserved: bool,
    exempt: bool,
}

impl LocalSite {
    /// A site is backlog unless it is pre-reserved by name or carries a
    /// written exemption. Anything the scan cannot attribute falls here, so
    /// ambiguity counts against the codebase rather than for it.
    fn is_backlog(&self) -> bool {
        !self.reserved && !self.exempt
    }
}

/// Splits discovered sites into `(reserved, exempt, backlog)`.
fn local_allocation_counts(sites: &[LocalSite]) -> (usize, usize, usize) {
    let reserved = sites.iter().filter(|site| site.reserved).count();
    let exempt = sites
        .iter()
        .filter(|site| site.exempt && !site.reserved)
        .count();
    (reserved, exempt, sites.len() - reserved - exempt)
}

/// Enumerates function-local collection allocation in the compositor's
/// scene-building and layout paths, and reports which sites are pre-reserved.
///
/// A site counts as reserved only when the collection it binds is itself
/// reserved: `let mut rows = Vec::new()` is converted when `rows.try_reserve`
/// appears in the same function body. Binding the check to the name is what
/// stops an unrelated `try_reserve` elsewhere in a long function from
/// absolving every other collection in it. A site the scan cannot attribute to
/// a binding — a `.to_vec()` returned directly, a `.collect()` passed straight
/// into a call — is reported as backlog, so ambiguity counts against the
/// codebase rather than for it.
fn scan_local_allocations(root: &Path) -> Vec<LocalSite> {
    let mut sites = Vec::new();
    for (source, text) in production_sources(root) {
        if !LOCAL_ALLOCATION_ROOTS
            .iter()
            .any(|prefix| source == *prefix || source.starts_with(prefix))
        {
            continue;
        }
        let code = mask_code(&text);
        for constructor in COLLECTION_CONSTRUCTORS {
            let mut cursor = 0usize;
            while let Some(found) = code[cursor..].find(constructor) {
                let offset = cursor + found;
                cursor = offset + constructor.len();
                let (function, body) = enclosing_function(&code, offset);
                let binding = binding_name(&code, offset);
                let reserved = binding.as_ref().is_some_and(|name| {
                    binding_is_reserved(&body, name)
                        || grown_through_helper(&code, &source, &body, name)
                });
                let exempt = LOCAL_EXEMPT.iter().any(|(path, owner, kind, bound, _)| {
                    *path == source
                        && *owner == function
                        && kind == constructor
                        && *bound == binding.as_deref().unwrap_or("-")
                });
                sites.push(LocalSite {
                    source: source.clone(),
                    offset,
                    line: code[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1,
                    function,
                    constructor: (*constructor).to_string(),
                    binding,
                    reserved,
                    exempt,
                });
            }
        }
    }
    sites.sort_by(|a, b| (&a.source, a.offset).cmp(&(&b.source, b.offset)));
    sites
}

/// Whether `name` is only grown through a declared fallible helper.
///
/// Looks for the binding passed as `&mut name` to a helper declared for this
/// file in [`LOCAL_FALLIBLE_HELPERS`]. The helper's own reservation is checked
/// separately by `check`, so a helper listed here cannot absolve a caller
/// unless it really does reserve.
fn grown_through_helper(code: &str, source: &str, body: &str, name: &str) -> bool {
    let borrow = format!("&mut {name}");
    LOCAL_FALLIBLE_HELPERS
        .iter()
        .filter(|(path, _, _)| *path == source)
        .any(|(_, helper, _)| {
            helper_reserves_parameter(code, helper).is_some()
                && call_arguments(body, helper).any(|arguments| {
                    arguments
                        .split(',')
                        .any(|argument| argument.trim() == borrow)
                })
        })
}

/// Whether `helper`'s body in `code` reserves its declared parameter.
fn helper_reserves_parameter<'a>(code: &'a str, helper: &str) -> Option<&'a str> {
    let body = function_body(code, helper)?;
    let (_, _, parameter) = LOCAL_FALLIBLE_HELPERS
        .iter()
        .find(|(_, name, _)| *name == helper)?;
    binding_is_reserved(body, parameter).then_some(body)
}

/// The argument lists of every call to `name` in `body`.
fn call_arguments<'a>(body: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    let call = format!("{name}(");
    let mut cursor = 0usize;
    std::iter::from_fn(move || {
        let found = body[cursor..].find(&call)? + cursor;
        let open = found + call.len();
        cursor = open;
        let mut depth = 1usize;
        for (offset, byte) in body.as_bytes()[open..].iter().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&body[open..open + offset]);
                    }
                }
                _ => {}
            }
        }
        // An unbalanced call ends the scan rather than guessing; the site
        // stays counted as backlog, which is the safe direction.
        None
    })
}

/// The tests a row names.
///
/// A row may name more than one, comma-separated: a collection's accounting
/// and its rejection path are different properties, and forcing a single name
/// made naming the second mean forgetting the first.
fn named_tests(field: &str) -> impl Iterator<Item = &str> {
    field
        .split(',')
        .map(str::trim)
        .filter(|test| !test.is_empty() && *test != "none")
}

/// Modules covered by the Miri gate, where a test that builds a tokio runtime
/// must be marked `#[cfg_attr(miri, ignore)]`.
///
/// Miri emulates no foreign functions, so `kqueue` is an *unsupported
/// operation* rather than a test failure: it aborts the whole run. One
/// unmarked async test therefore takes the entire gate down, and does it
/// twenty minutes in. The check below is what stops that happening by
/// surprise.
/// The four lints that make a panicking operation on remote input a compile
/// error. Named once so the gate, the marker and the error message cannot
/// disagree about which class is being denied.
const PANIC_LINTS: &[&str] = &[
    "clippy::indexing_slicing",
    "clippy::unwrap_used",
    "clippy::expect_used",
    "clippy::panic",
];

/// Per production crate: whether its root denies the panic lints, and how many
/// `#[allow]`s of those lints it carries without a `reason`.
///
/// The deny block is the whole mechanism, so its presence is checked rather
/// than assumed. Residue is counted rather than tabulated: a second exemption
/// table would be a standing argument someone has to re-check, whereas an
/// `#[allow(..., reason = "...")]` carries its argument at the site.
///
/// This gate exists because `trim_owned_string` panicked on whitespace-only
/// element content, reachable from remote AML through `[button]`, `[option]`
/// and `[text-animate]`. A fuzz smoke run found it; nothing else did, and
/// nothing else could have. The reason was a scope gap rather than an
/// oversight: the registry discovers heap-owning structs, the backlog scan
/// finds function-local collection constructors, hook coverage checks that
/// governed rows have rejection hooks, and enforcement classification labels
/// claims. All four ask what remote content may *allocate*; none asked what it
/// may *index*. `value.drain(..start)` allocates nothing, so it was invisible
/// to the entire program. `forbid(unsafe_code)` bounds the severity to an
/// abort rather than memory corruption, but an abort reachable from a remote
/// byte is still denial of service against the malicious-AML and
/// remote-ATP-server adversaries in `verification/threat-model.json`.
fn panic_gate_status(root: &Path) -> Vec<(String, bool, usize)> {
    let mut status = Vec::new();
    let Ok(entries) = fs::read_dir(root.join("crates")) else {
        return status;
    };
    let mut members: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    members.sort();
    for member in members {
        let name = member
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let root_file = ["lib.rs", "main.rs"]
            .iter()
            .map(|file| member.join("src").join(file))
            .find(|path| path.is_file());
        let Some(root_file) = root_file else {
            continue;
        };
        let Ok(text) = fs::read_to_string(&root_file) else {
            continue;
        };
        let gated =
            text.contains("not(test),") && PANIC_LINTS.iter().all(|lint| text.contains(lint));

        let mut unreasoned = 0;
        for (_, text) in crate_sources(&member) {
            let masked = mask_literals(&text);
            for (index, line) in masked.lines().enumerate() {
                let trimmed = line.trim_start();
                if !trimmed.starts_with("#[allow(") && !trimmed.starts_with("#![allow(") {
                    continue;
                }
                if !PANIC_LINTS.iter().any(|lint| trimmed.contains(lint)) {
                    continue;
                }
                // A `reason` may sit on a later line of a wrapped attribute,
                // so read to the closing bracket rather than this line alone.
                let mut attribute = String::new();
                for line in masked.lines().skip(index) {
                    attribute.push_str(line);
                    attribute.push(' ');
                    if line.contains(")]") {
                        break;
                    }
                }
                if !attribute.contains("reason") {
                    unreasoned += 1;
                }
            }
        }
        status.push((name, gated, unreasoned));
    }
    status
}

/// Every `.rs` file under one crate, by path and text.
fn crate_sources(member: &Path) -> Vec<(String, String)> {
    fn visit(path: &Path, output: &mut Vec<(String, String)>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                visit(&path, output);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
                && let Ok(text) = fs::read_to_string(&path)
            {
                output.push((path.to_string_lossy().into_owned(), text));
            }
        }
    }
    let mut output = Vec::new();
    for directory in ["src", "tests"] {
        let directory = member.join(directory);
        if directory.is_dir() {
            visit(&directory, &mut output);
        }
    }
    output
}

/// The campaign log, and the targets it must account for.
const FUZZ_LOG: &str = "verification/fuzz-campaign.tsv";
const FUZZ_TARGET_DIR: &str = "fuzz/fuzz_targets";

/// Fuzz targets with no campaign row for the current workspace version.
///
/// "Run it for hours" is unrepeatable and so cannot be a closure condition.
/// A row per target per version is: it says how long the target actually ran,
/// how many executions that bought, and on what host and toolchain — without
/// which an execution count means nothing. Adding a fuzz target therefore
/// fails the build until it has been run.
///
/// The version is read from the workspace manifest rather than passed in, so
/// bumping the version invalidates every row at once. That is the intended
/// behaviour: a campaign against different code is not evidence about this
/// code.
fn uncampaigned_fuzz_targets(root: &Path) -> Vec<String> {
    let Ok(manifest) = fs::read_to_string(root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Some(version) = manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.split('"').next())
    else {
        return vec!["<workspace version unreadable>".to_string()];
    };

    let mut targets = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join(FUZZ_TARGET_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                targets.push(stem.to_string());
            }
        }
    }
    targets.sort();

    let log = fs::read_to_string(root.join(FUZZ_LOG)).unwrap_or_default();
    let campaigned: BTreeSet<&str> = log
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let row_version = fields.next()?;
            let target = fields.next()?;
            (row_version == version).then_some(target)
        })
        .collect();

    targets
        .into_iter()
        .filter(|target| !campaigned.contains(target.as_str()))
        .collect()
}

/// Whether one line is a `#[cfg]` attribute conditional on the target OS.
///
/// Split out so the unit test exercises the same predicate the walk does; a
/// test that restates the condition passes when the condition drifts.
fn line_gates_on_platform(line: &str) -> bool {
    let trimmed = line.trim_start();
    (trimmed.starts_with("#[cfg(") || trimmed.starts_with("#![cfg("))
        && trimmed.contains("target_os")
}

/// Items excluded from compilation on a platform, by `file:line`.
///
/// The Linux claim rests on Linux code being *compiled* by every macOS gate
/// run. Code behind `#[cfg(target_os = ...)]` is not: it is invisible to
/// `cargo clippy --all-targets` on the other platform and rots silently
/// between runs, so a failure on the first Linux run is indistinguishable
/// from months of drift.
///
/// Only conditional *compilation* is reported. `cfg!(target_os = ...)` as an
/// expression and `#[cfg_attr(target_os = ..., ignore = "...")]` both keep the
/// code type-checked on both platforms and are the intended replacements.
/// `cfg(unix)` splits compile on both claimed platforms and are not counted.
fn platform_gated_items(root: &Path) -> Vec<String> {
    fn visit(base: &Path, path: &Path, output: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                visit(base, &path, output);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let (Ok(text), Ok(relative)) = (fs::read_to_string(&path), path.strip_prefix(base))
            else {
                continue;
            };
            let source = relative.to_string_lossy().replace('\\', "/");
            for (index, line) in mask_literals(&text).lines().enumerate() {
                if line_gates_on_platform(line) {
                    output.push(format!("{source}:{}", index + 1));
                }
            }
        }
    }
    let mut output = Vec::new();
    let Ok(entries) = fs::read_dir(root.join("crates")) else {
        return output;
    };
    let mut members: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    members.sort();
    for member in members {
        for directory in ["src", "tests"] {
            let directory = member.join(directory);
            if directory.is_dir() {
                visit(root, &directory, &mut output);
            }
        }
    }
    output
}

const MIRI_GATED_ROOTS: &[&str] = &["crates/dustnet-client/src/compositor/"];

/// Async tests under a Miri-gated root that are not excluded from Miri.
///
/// Works line by line over the attribute run attached to each `#[tokio::test]`
/// — from the first attribute line above it to the `fn` below — rather than a
/// byte window, because a fixed lookback reaches into the *previous* test's
/// attributes and reads its exclusion as this one's.
fn unmarked_async_tests(root: &Path) -> Vec<String> {
    let mut unmarked = Vec::new();
    for (source, text) in all_crate_sources(root) {
        if !MIRI_GATED_ROOTS
            .iter()
            .any(|prefix| source.starts_with(prefix))
        {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if line.trim() != "#[tokio::test]" {
                continue;
            }
            let mut first = index;
            while first > 0 && lines[first - 1].trim_start().starts_with("#[") {
                first -= 1;
            }
            let mut last = index;
            while last + 1 < lines.len() && lines[last + 1].trim_start().starts_with("#[") {
                last += 1;
            }
            if lines[first..=last]
                .iter()
                .any(|line| line.contains("cfg_attr(miri"))
            {
                continue;
            }
            let name = lines
                .get(last + 1)
                .and_then(|line| line.split("fn ").nth(1))
                .and_then(|rest| rest.split(['(', '<']).next())
                .unwrap_or("<unknown>");
            unmarked.push(format!("{source}::{name}"));
        }
    }
    unmarked
}

// Architectural claims and how each is actually enforced. Two tables rather
// than one enum field, because the two classes carry different evidence:
// structural claims cite the artifact that makes the violation
// unrepresentable, lint-enforced claims cite the test that searches for it.
//
// `Structural` means the violation cannot be written — a crate-graph
// partition, a compiler-enforced lint level, an import-graph constraint.
// `Lint` means the violation is merely unspelled: a test that reads sibling
// sources and asserts a string is absent, which breaks on a rename and passes
// on a semantically equivalent bypass.

/// Claims enforced structurally, each with the artifact that enforces it.
///
/// The evidence is a `(path, token)` pair that must be present: the mechanism
/// is configuration or a graph constraint, not a search for the absence of a
/// bypass. A claim listed here must not also be implemented by a source-text
/// search, which `check` verifies against the discovered lint set.
const STRUCTURAL_ENFORCED: &[(&str, &str, &str)] = &[
    (
        "core depends on neither client nor server, and the production server \
         cannot reach the unsupported social example",
        "Makefile",
        "ci-boundaries:",
    ),
    (
        "unsafe Rust is unrepresentable in production crates",
        "Cargo.toml",
        "unsafe_code = \"forbid\"",
    ),
    (
        "animation construction and ticking cannot reach the network or the \
         viewer reducer",
        "tools/allocation-audit/src/main.rs",
        "const MODULE_ISOLATION",
    ),
    (
        "no heap-owning production struct exists without a classification",
        "tools/allocation-audit/src/main.rs",
        "fn scan_registry",
    ),
    (
        "no function-local collection in the scanned compositor modules grows \
         without a reservation",
        "tools/allocation-audit/src/main.rs",
        "fn scan_local_allocations",
    ),
    (
        "every governed row names a test that drives an owner-keyed rejection \
         hook, and no hook's state survives into a release build",
        "tools/allocation-audit/src/main.rs",
        "fn hook_coverage",
    ),
];

/// Claims enforced only by a source-text search, each naming the test.
///
/// Every test discovered to search sibling sources must appear here. That is
/// what stops a new substring assertion being added without saying so, and
/// stops one being described as structural later: `check` fails on a
/// discovered searcher that is undeclared, and on a claim that appears in both
/// tables.
const LINT_ENFORCED: &[(&str, &str)] = &[
    (
        "the reducer is called only by the terminal dispatcher",
        "reducer_is_only_called_by_the_terminal_dispatcher",
    ),
    (
        "the viewer is the only issuer of lifecycle identities",
        "viewer_is_the_only_lifecycle_identity_issuer",
    ),
    (
        "the terminal loop cannot mutate the active scene",
        "terminal_loop_cannot_mutate_the_active_scene",
    ),
];

/// Tests that read sibling sources and assert over their text.
///
/// Discovery is by mechanism, not by name or module: a test that calls
/// `include_str!` is reading source text, and any claim it makes is therefore
/// a lint. Nothing here depends on the test living in a module called
/// `architecture_tests`.
fn source_searching_tests(root: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (_, text) in production_sources(root) {
        let code = mask_literals(&text);
        let mut cursor = 0usize;
        while let Some(offset) = code[cursor..].find("include_str!") {
            let position = cursor + offset;
            cursor = position + "include_str!".len();
            let (name, _) = enclosing_function(&code, position);
            if name != "<none>" {
                found.insert(name);
            }
        }
    }
    found
}

/// The threat model, as data.
///
/// `verification/threat-model.json` is authoritative for the adversaries assumed
/// hostile and for the control and evidence answering each. Coverage nests
/// inside the adversary it answers, so an answer to an unlisted adversary is
/// not representable and needs no check.
///
/// The control column is **not** checked against `docs/spec/07-security.md`.
/// Nothing in this repository parses documentation: prose is prose, and every
/// machine-checked claim lives under `verification/` as data. The column names
/// which control answers the threat, for a reader; the binding with teeth is
/// the evidence column, which must resolve to a real test or make target.
const THREAT_MODEL: &str = "verification/threat-model.json";

#[derive(serde::Deserialize)]
struct ThreatModel {
    properties: Vec<String>,
    adversaries: Vec<Adversary>,
}

#[derive(serde::Deserialize)]
struct Adversary {
    name: String,
    coverage: Vec<Coverage>,
}

#[derive(serde::Deserialize)]
struct Coverage {
    property: String,
    evidence: String,
}

fn threat_model(root: &Path) -> Result<ThreatModel, String> {
    let raw = fs::read_to_string(root.join(THREAT_MODEL))
        .map_err(|error| format!("read {THREAT_MODEL}: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("parse {THREAT_MODEL}: {error}"))
}

/// Governed-row rejection coverage: which `governed` rows name a test that
/// actually drives a rejection path through an owner-keyed hook.
///
/// The plan previously asserted which modules had hooks and which did not.
/// That claim was already wrong — `compositor/scene` and `compositor/animate`
/// both had hooks it said they lacked — which is exactly why the count is
/// generated here rather than written down.
///
/// A row counts as covered when the test it names mentions a discovered hook
/// symbol as a whole identifier. The rule deliberately does not require the
/// hook to live in the row's own file: a row in `scene/tree.rs` can legitimately
/// be exercised through a hook armed in `scene/build.rs`.
struct HookCoverage {
    symbols: BTreeSet<String>,
    bare: Vec<String>,
}

fn hook_coverage(root: &Path, rows: &BTreeMap<(String, String, String), Row>) -> HookCoverage {
    let mut symbols = BTreeSet::new();
    for (_, text) in production_sources(root) {
        symbols.extend(rejection_hooks(&text));
    }
    let corpus = rust_source_corpus(&root.join("crates"));
    let mut bare = Vec::new();
    for row in rows.values().filter(|row| row.status == "governed") {
        let covered = named_tests(&row.tests).any(|test| {
            function_body(&corpus, test)
                .is_some_and(|body| symbols.iter().any(|symbol| mentions_token(body, symbol)))
        });
        if !covered {
            bare.push(format!("{}\t{}", row.id, row.tests));
        }
    }
    bare.sort();
    HookCoverage { symbols, bare }
}

/// A test-only allocation-rejection hook discovered in a source file.
///
/// Discovery is **structural**: a hook is a `#[cfg(test)]` thread-local `Cell`,
/// together with any `#[cfg(test)]` function or `impl` that writes to one.
/// Nothing keys off the `REJECT_` naming convention, so a renamed hook is
/// still found and a production static that merely reads like one is not.
///
/// The symbols are what a test names to arm the hook, which is what makes
/// "this test drives a rejection path" checkable.
fn rejection_hooks(source: &str) -> BTreeSet<String> {
    let items = cfg_test_items(source);
    let mut statics = BTreeSet::new();
    for item in &items {
        let mut rest = item.as_str();
        while let Some(offset) = rest.find("static ") {
            let after = &rest[offset + "static ".len()..];
            rest = after;
            let name: String = after
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            let Some(colon) = after.find(':') else {
                continue;
            };
            // The declared type runs from the colon to the initialiser, or to
            // the end of the statement if there is none. Searching for `=`
            // from the start of the item instead would find one *before* the
            // colon and invert the range.
            let tail = &after[colon..];
            let stop = tail
                .find('=')
                .into_iter()
                .chain(tail.find(';'))
                .min()
                .unwrap_or(tail.len());
            if !name.is_empty() && tail[..stop].contains("Cell") {
                statics.insert(name);
            }
        }
    }

    let mut symbols = statics.clone();
    for item in &items {
        if !statics.iter().any(|name| mentions_token(item, name)) {
            continue;
        }
        if let Some(name) = declared_item_name(item) {
            symbols.insert(name);
        }
    }
    symbols
}

/// The text of every `#[cfg(test)]` item in a source file, attribute excluded.
fn cfg_test_items(source: &str) -> Vec<String> {
    const ATTRIBUTE: &str = "#[cfg(test)]";
    let bytes = source.as_bytes();
    let mut items = Vec::new();
    let mut index = 0usize;
    while let Some(found) = source[index..].find(ATTRIBUTE) {
        let start = index + found;
        let after = start + ATTRIBUTE.len();
        let brace = source[after..].find('{').map(|found| after + found);
        let semicolon = source[after..].find(';').map(|found| after + found);
        let block_first = match (brace, semicolon) {
            (Some(brace), Some(semicolon)) => brace < semicolon,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let end = match (block_first, brace, semicolon) {
            (true, Some(brace), _) => match_brace(bytes, brace).unwrap_or(bytes.len()),
            (false, _, Some(semicolon)) => semicolon + 1,
            _ => break,
        };
        items.push(source[after..end].to_string());
        index = end;
    }
    items
}

/// The name a `#[cfg(test)]` item introduces: the function it declares, or the
/// type an `impl` block is for. `thread_local!` blocks introduce no callable
/// name and are skipped — their statics are collected separately.
fn declared_item_name(item: &str) -> Option<String> {
    let head = &item[..item.find('{').unwrap_or(item.len())];
    for keyword in ["fn ", "impl "] {
        if let Some(offset) = head.find(keyword) {
            let after = &head[offset + keyword.len()..];
            let name: String = after
                .trim_start()
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Whether `text` contains `token` as a whole identifier.
fn mentions_token(text: &str, token: &str) -> bool {
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while let Some(found) = text[cursor..].find(token) {
        let start = cursor + found;
        let end = start + token.len();
        cursor = end;
        let before_ok = start
            .checked_sub(1)
            .is_none_or(|before| !bytes[before].is_ascii_alphanumeric() && bytes[before] != b'_');
        let after_ok = bytes
            .get(end)
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_');
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// An offset-preserving mask of a source file: comments, string and character
/// literals, and `#[cfg(test)]` items become spaces while every newline is
/// kept.
///
/// Scanning the mask rather than the text means a constructor named in a
/// comment or a doc example cannot register as an allocation site — the same
/// reason `MODULE_ISOLATION` walks imports instead of searching for strings —
/// and preserving offsets keeps reported line numbers pointing at the real
/// file.
fn mask_code(source: &str) -> String {
    mask_cfg_test(&mask_literals(source))
}

/// Masks comments and string/character literals, keeping every byte offset and
/// every `#[cfg(test)]` item. Callers that need to see inside test modules use
/// this directly; [`mask_code`] additionally blanks those modules.
fn mask_literals(source: &str) -> String {
    fn blank(mask: &mut [u8], from: usize, to: usize) {
        for byte in &mut mask[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    let bytes = source.as_bytes();
    let mut mask = bytes.to_vec();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            let end = source[index..]
                .find('\n')
                .map_or(bytes.len(), |found| index + found);
            blank(&mut mask, index, end);
            index = end;
        } else if bytes[index..].starts_with(b"/*") {
            let mut depth = 0usize;
            let mut end = bytes.len();
            let mut scan = index;
            while scan < bytes.len() {
                if bytes[scan..].starts_with(b"/*") {
                    depth += 1;
                    scan += 2;
                } else if bytes[scan..].starts_with(b"*/") {
                    depth -= 1;
                    scan += 2;
                    if depth == 0 {
                        end = scan;
                        break;
                    }
                } else {
                    scan += 1;
                }
            }
            blank(&mut mask, index, end);
            index = end;
        } else if bytes[index] == b'"' {
            let mut scan = index + 1;
            while scan < bytes.len() {
                if bytes[scan] == b'\\' {
                    scan += 2;
                } else if bytes[scan] == b'"' {
                    scan += 1;
                    break;
                } else {
                    scan += 1;
                }
            }
            let end = scan.min(bytes.len());
            blank(&mut mask, index, end);
            index = end;
        } else if bytes[index] == b'\'' && is_char_literal(bytes, index) {
            let end = char_literal_end(bytes, index);
            blank(&mut mask, index, end);
            index = end;
        } else {
            index += 1;
        }
    }
    String::from_utf8(mask).unwrap_or_else(|_| source.to_string())
}

/// Distinguishes a character literal from a lifetime: `'a` in `&'a Scene` is
/// followed by an identifier and no closing quote.
fn is_char_literal(bytes: &[u8], index: usize) -> bool {
    match bytes.get(index + 1) {
        Some(b'\\') => true,
        Some(_) => bytes.get(index + 2) == Some(&b'\''),
        None => false,
    }
}

fn char_literal_end(bytes: &[u8], index: usize) -> usize {
    let mut scan = index + 1;
    while scan < bytes.len() {
        if bytes[scan] == b'\\' {
            scan += 2;
        } else if bytes[scan] == b'\'' {
            return scan + 1;
        } else {
            scan += 1;
        }
    }
    bytes.len()
}

/// Blanks `#[cfg(test)]` items in place, keeping every byte offset. This is
/// the offset-preserving counterpart of [`strip_cfg_test`], which discovery
/// uses where offsets do not matter.
fn mask_cfg_test(source: &str) -> String {
    const ATTRIBUTE: &str = "#[cfg(test)]";
    let bytes = source.as_bytes();
    let mut mask = bytes.to_vec();
    let mut index = 0usize;
    while let Some(found) = source[index..].find(ATTRIBUTE) {
        let start = index + found;
        let after = start + ATTRIBUTE.len();
        let brace = source[after..].find('{').map(|found| after + found);
        let semicolon = source[after..].find(';').map(|found| after + found);
        let block_first = match (brace, semicolon) {
            (Some(brace), Some(semicolon)) => brace < semicolon,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let end = match (block_first, brace, semicolon) {
            (true, Some(brace), _) => match_brace(bytes, brace).unwrap_or(bytes.len()),
            (false, _, Some(semicolon)) => semicolon + 1,
            _ => break,
        };
        for byte in &mut mask[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        index = end;
    }
    String::from_utf8(mask).unwrap_or_else(|_| source.to_string())
}

/// The offset just past the `}` closing the block that opens at `open`.
fn match_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The name and body of the innermost `fn` containing `offset`.
///
/// Walks candidate declarations from the closest backwards and takes the first
/// whose body actually spans the offset, so a preceding sibling function does
/// not claim a site that belongs to the one after it.
fn enclosing_function(code: &str, offset: usize) -> (String, String) {
    let bytes = code.as_bytes();
    let mut starts: Vec<usize> = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = code[cursor..offset.min(code.len())].find("fn ") {
        let start = cursor + found;
        cursor = start + 3;
        let preceded_by_ident = start
            .checked_sub(1)
            .is_some_and(|before| bytes[before].is_ascii_alphanumeric() || bytes[before] == b'_');
        if !preceded_by_ident {
            starts.push(start);
        }
    }
    for start in starts.into_iter().rev() {
        let Some(open) = code[start..].find('{').map(|found| start + found) else {
            continue;
        };
        let Some(end) = match_brace(bytes, open) else {
            continue;
        };
        if open < offset && offset < end {
            let name: String = code[start + 3..]
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            return (name, code[open..end].to_string());
        }
    }
    (String::from("<none>"), String::new())
}

/// Whether the collection bound to `name` is pre-reserved somewhere in the
/// enclosing function body.
///
/// Matches `name` as a whole token and then skips whitespace, so the wrapped
/// form rustfmt produces —
///
/// ```text
/// focusable_positions
///     .try_reserve_exact(count)
/// ```
///
/// — counts the same as the single-line form. A check that depended on the
/// two being adjacent would silently report a converted site as backlog every
/// time a line grew past the formatter's width.
fn binding_is_reserved(body: &str, name: &str) -> bool {
    let bytes = body.as_bytes();
    let mut cursor = 0usize;
    while let Some(found) = body[cursor..].find(name) {
        let start = cursor + found;
        let end = start + name.len();
        cursor = end;
        let boundary_before = start
            .checked_sub(1)
            .is_none_or(|before| !bytes[before].is_ascii_alphanumeric() && bytes[before] != b'_');
        if !boundary_before {
            continue;
        }
        if body[end..].trim_start().starts_with(".try_reserve") {
            return true;
        }
    }
    false
}

/// The variable a constructor is bound to, if the site is a `let` statement.
fn binding_name(code: &str, offset: usize) -> Option<String> {
    let start = code[..offset]
        .rfind([';', '{', '}'])
        .map_or(0, |found| found + 1);
    let statement = code[start..offset].trim_start();
    let rest = statement.strip_prefix("let ")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fuzz target has been run against this version, with the result
    /// recorded. A new target fails here until a campaign has covered it.
    #[test]
    fn every_fuzz_target_has_a_campaign_row_for_this_version() {
        assert_eq!(
            uncampaigned_fuzz_targets(&repository_root()),
            Vec::<String>::new()
        );
    }

    /// Panic-freedom's own closure condition, asserted on the tree: every
    /// production crate denies the four lints at its root, and no crate
    /// carries an exemption without an argument attached to it.
    #[test]
    fn every_production_crate_denies_the_panic_lints_with_no_bare_exemption() {
        let status = panic_gate_status(&repository_root());
        assert!(!status.is_empty(), "no crates were inspected");
        let ungated: Vec<&str> = status
            .iter()
            .filter(|(_, gated, _)| !*gated)
            .map(|(name, _, _)| name.as_str())
            .collect();
        assert_eq!(ungated, Vec::<&str>::new());
        let unreasoned: usize = status.iter().map(|(_, _, unreasoned)| unreasoned).sum();
        assert_eq!(unreasoned, 0);
    }

    /// The Linux claim's precondition, asserted on the tree rather than
    /// described: nothing under `crates/*/{src,tests}` is excluded from
    /// compilation by platform. If this fails, some Linux path has not been
    /// type-checked by any macOS gate run since it was written.
    #[test]
    fn no_crate_source_is_excluded_from_compilation_by_platform() {
        assert_eq!(
            platform_gated_items(&repository_root()),
            Vec::<String>::new()
        );
    }

    /// The check must see a `#[cfg]` attribute and must not see the two
    /// permitted forms, which keep both branches compiled everywhere.
    #[test]
    fn platform_gating_distinguishes_compilation_from_selection() {
        let source = "\
#[cfg(target_os = \"linux\")]
fn gated() {}
#[cfg_attr(target_os = \"macos\", ignore = \"linux driver shape\")]
fn skipped() {}
fn selected() { let _ = cfg!(target_os = \"macos\"); }
";
        let hits: Vec<usize> = mask_literals(source)
            .lines()
            .enumerate()
            .filter(|(_, line)| line_gates_on_platform(line))
            .map(|(index, _)| index + 1)
            .collect();
        assert_eq!(hits, vec![1]);
    }

    #[test]
    fn finds_nested_heap_fields_without_counting_scalars() {
        let source = "struct Demo { scalar: u64, names: Vec<String>, maybe: Option<Arc<str>> }";
        assert_eq!(
            heap_fields(source, "Demo").unwrap(),
            BTreeSet::from(["maybe".to_string(), "names".to_string()])
        );
    }

    #[test]
    fn finds_registered_function_bodies() {
        let source = "impl Demo { async fn send(&mut self) { if true { work(); } } }";
        assert_eq!(
            function_body(source, "send").map(str::trim),
            Some("if true { work(); }")
        );
    }

    #[test]
    fn unit_and_tuple_structs_do_not_borrow_the_next_items_fields() {
        // `struct NodeId;` inside a macro used to make the scanner walk forward
        // to the next `{` and attribute that struct's fields to the unit one.
        let source = "struct Marker; struct Real { names: Vec<String> }";
        assert!(heap_fields(source, "Marker").unwrap().is_empty());
        assert_eq!(
            heap_fields(source, "Real").unwrap(),
            BTreeSet::from(["names".to_string()])
        );
        let tuple = "struct Key(u32, String);";
        assert_eq!(
            heap_fields(tuple, "Key").unwrap(),
            BTreeSet::from(["1".to_string()])
        );
    }

    #[test]
    fn borrowed_fields_are_not_heap_owners() {
        let source = "struct View<'a> { scene: &'a Scene, stack: Vec<u32> }";
        assert_eq!(
            heap_fields(source, "View").unwrap(),
            BTreeSet::from(["stack".to_string()])
        );
    }

    #[test]
    fn discovery_skips_cfg_test_items() {
        let source =
            "struct Live { a: Vec<u8> } #[cfg(test)] mod tests { struct Fake { b: Vec<u8> } }";
        assert_eq!(declared_structs(&strip_cfg_test(source)), vec!["Live"]);
    }

    #[test]
    fn every_exemption_names_a_reason() {
        for (source, name, reason) in EXEMPT {
            assert!(!reason.trim().is_empty(), "{source}::{name} has no reason");
        }
    }

    #[test]
    fn repository_registry_is_closed() {
        let (_, unclassified, _) = scan_registry(&repository_root());
        let names: Vec<_> = unclassified
            .iter()
            .map(|owner| format!("{}::{}", owner.source, owner.name))
            .collect();
        assert!(names.is_empty(), "unclassified heap owners: {names:?}");
    }

    #[test]
    fn crate_imports_reads_every_use_form() {
        let flat = crate_imports("use crate::client::AtpClient;\npub use crate::a::B;");
        assert!(flat.contains("crate::client::AtpClient"), "{flat:?}");
        assert!(flat.contains("crate::a::B"), "{flat:?}");

        // Grouped directly under `crate::` — the form a naive leading-run
        // parser drops silently, which would be a hole in the isolation check.
        let grouped = crate_imports("use crate::{client::AtpClient, viewer::PageScope};");
        assert!(grouped.contains("crate::client::AtpClient"), "{grouped:?}");
        assert!(grouped.contains("crate::viewer::PageScope"), "{grouped:?}");

        // Multi-line, nested, and prefixed groups.
        let nested = crate_imports("use crate::{\n    client::{A, B},\n    transport::C,\n};");
        assert!(nested.contains("crate::client"), "{nested:?}");
        assert!(nested.contains("crate::transport::C"), "{nested:?}");
        let multiline = crate_imports("use crate::parser::ast::{\n    Easing,\n    Keyframe,\n};");
        assert!(multiline.contains("crate::parser::ast"), "{multiline:?}");

        // Commented-out imports are not imports.
        assert!(crate_imports("// use crate::client::AtpClient;").is_empty());
        assert!(crate_imports(" * use crate::client::AtpClient;").is_empty());
    }

    #[test]
    fn repository_module_isolation_holds() {
        let mut errors = Vec::new();
        check_module_isolation(&repository_root(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn mask_code_blanks_comments_and_strings_without_moving_offsets() {
        let source = "let a = 1; // Vec::new()\nlet b = \"Vec::new()\";\nlet c = Vec::new();";
        let masked = mask_code(source);
        assert_eq!(masked.len(), source.len(), "offsets must be preserved");
        assert_eq!(
            masked.matches("Vec::new(").count(),
            1,
            "only the real constructor survives: {masked}"
        );
        assert_eq!(masked.lines().count(), source.lines().count());
    }

    #[test]
    fn mask_code_keeps_lifetimes_and_blanks_cfg_test_items() {
        // `'a` is a lifetime, not an unterminated character literal: treating
        // it as one would swallow the rest of the file and hide every site
        // after it.
        let source = "fn f<'a>(s: &'a str) { let v = Vec::new(); }\n#[cfg(test)]\nmod t { let w = Vec::new(); }";
        let masked = mask_code(source);
        assert_eq!(masked.len(), source.len());
        assert_eq!(masked.matches("Vec::new(").count(), 1, "{masked}");
    }

    #[test]
    fn binding_is_reserved_tolerates_wrapped_formatting() {
        assert!(binding_is_reserved("names.try_reserve_exact(n)?;", "names"));
        assert!(binding_is_reserved(
            "names\n        .try_reserve_exact(n)\n        .ok()?;",
            "names"
        ));
        // A different collection's reservation must not absolve this one.
        assert!(!binding_is_reserved(
            "other.try_reserve_exact(n)?;",
            "names"
        ));
        // Nor may a longer name that merely contains it.
        assert!(!binding_is_reserved(
            "names_index.try_reserve(n)?;",
            "names"
        ));
    }

    #[test]
    fn enclosing_function_picks_the_body_that_contains_the_site() {
        let source = "fn first() { let a = 1; }\nfn second() { let b = Vec::new(); }";
        let offset = source.find("Vec::new(").unwrap();
        let (name, body) = enclosing_function(source, offset);
        assert_eq!(name, "second");
        assert!(body.contains("let b"), "{body}");
    }

    #[test]
    fn binding_name_reads_let_bindings_only() {
        assert_eq!(
            binding_name("let mut rows = Vec::new();", "let mut rows = ".len()),
            Some("rows".to_string())
        );
        assert_eq!(
            binding_name(
                "let rows: Vec<u8> = Vec::new();",
                "let rows: Vec<u8> = ".len()
            ),
            Some("rows".to_string())
        );
        assert_eq!(binding_name("push(Vec::new());", "push(".len()), None);
    }

    #[test]
    fn every_local_exemption_names_a_reason() {
        for (source, function, constructor, _, reason) in LOCAL_EXEMPT {
            assert!(
                !reason.trim().is_empty(),
                "{source}::{function} `{constructor}` has no reason"
            );
        }
    }

    #[test]
    fn repository_local_backlog_is_enumerated_not_estimated() {
        let sites = scan_local_allocations(&repository_root());
        assert!(
            !sites.is_empty(),
            "the scan found nothing; a path change has silently disabled it"
        );
        let (_, _, backlog) = local_allocation_counts(&sites);
        // Not an assertion about the size of the backlog: the count lives in
        // the gate marker, which `check` enforces. This asserts only that
        // the scan is still attributing sites rather than returning nothing.
        assert!(backlog <= sites.len());
    }

    #[test]
    fn call_arguments_reads_nested_parentheses() {
        let body = "try_push_span(&mut spans, InlineSpan { width: w(1, 2) })?;";
        let found: Vec<_> = call_arguments(body, "try_push_span").collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("&mut spans"), "{found:?}");
        assert!(found[0].ends_with("}"), "{found:?}");
        // The nested `w(1, 2)` must not terminate the argument list early.
        assert!(found[0].contains("w(1, 2)"), "{found:?}");
    }

    #[test]
    fn declared_fallible_helpers_really_reserve() {
        // The point of declaring a helper is that callers stop spelling the
        // reservation. That is only safe if the helper still performs it, so
        // the declaration is checked rather than believed.
        for (source, function, parameter) in LOCAL_FALLIBLE_HELPERS {
            let text = fs::read_to_string(repository_root().join(source))
                .unwrap_or_else(|error| panic!("read {source}: {error}"));
            assert!(
                helper_reserves_parameter(&mask_code(&text), function).is_some(),
                "{source}::{function} no longer reserves `{parameter}`"
            );
        }
    }

    #[test]
    fn a_helper_that_stops_reserving_is_rejected() {
        let reserving = "fn try_push_span(spans: &mut Vec<u8>, v: u8) -> Option<()> { \
                         spans.try_reserve(1).ok()?; spans.push(v); Some(()) }";
        assert!(helper_reserves_parameter(reserving, "try_push_span").is_some());
        let bare = "fn try_push_span(spans: &mut Vec<u8>, v: u8) -> Option<()> { \
                    spans.push(v); Some(()) }";
        assert!(helper_reserves_parameter(bare, "try_push_span").is_none());
    }

    #[test]
    fn repository_local_backlog_is_closed() {
        let sites = scan_local_allocations(&repository_root());
        let outstanding: Vec<_> = sites
            .iter()
            .filter(|site| site.is_backlog())
            .map(|site| format!("{}:{} {}", site.source, site.line, site.constructor))
            .collect();
        assert!(outstanding.is_empty(), "local backlog: {outstanding:?}");
    }

    #[test]
    fn rejection_hooks_are_discovered_without_matching_on_names() {
        // Nothing keys off `REJECT_`: the hook here is named after fruit and
        // is still found, because discovery follows `#[cfg(test)]` items and
        // the `Cell` they hold.
        let source = "#[cfg(test)]\nthread_local! {\n    static MANGO: std::cell::Cell<bool> = \
                      const { std::cell::Cell::new(false) };\n}\n\
                      #[cfg(test)]\npub(crate) fn arm_it() { MANGO.with(|c| c.set(true)); }";
        let hooks = rejection_hooks(source);
        assert!(hooks.contains("MANGO"), "{hooks:?}");
        assert!(hooks.contains("arm_it"), "{hooks:?}");
    }

    #[test]
    fn a_production_static_is_not_mistaken_for_a_hook() {
        let source = "thread_local! {\n    static REJECT_EVERYTHING: std::cell::Cell<bool> = \
                      const { std::cell::Cell::new(false) };\n}";
        assert!(
            rejection_hooks(source).is_empty(),
            "a production static must not count as a test hook"
        );
    }

    #[test]
    fn named_tests_splits_a_list_and_drops_none() {
        let listed: Vec<_> = named_tests("accounting_test, rejection_test").collect();
        assert_eq!(listed, vec!["accounting_test", "rejection_test"]);
        assert_eq!(named_tests("none").count(), 0);
    }

    #[test]
    fn no_rejection_hook_state_survives_into_production() {
        for (source, text) in production_sources(&repository_root()) {
            let production = strip_cfg_test(&text);
            for symbol in rejection_hooks(&text) {
                if symbol
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch == '_')
                {
                    assert!(
                        !mentions_token(&production, &symbol),
                        "{source}::{symbol} would exist in a release build"
                    );
                }
            }
        }
    }

    #[test]
    fn every_governed_row_drives_a_rejection_hook() {
        let rows =
            read_inventory(&repository_root().join("verification/allocation-owners.tsv")).unwrap();
        let indexed: BTreeMap<_, _> = rows
            .into_iter()
            .map(|row| {
                (
                    (row.source.clone(), row.owner.clone(), row.field.clone()),
                    row,
                )
            })
            .collect();
        let coverage = hook_coverage(&repository_root(), &indexed);
        assert!(
            coverage.bare.is_empty(),
            "governed rows without rejection coverage: {:?}",
            coverage.bare
        );
    }

    #[test]
    fn source_searching_tests_are_discovered_by_mechanism() {
        // Discovery keys off the call, not the module name or the test name.
        let found = source_searching_tests(&repository_root());
        for (claim, test) in LINT_ENFORCED {
            assert!(
                found.contains(*test),
                "lint-enforced claim \"{claim}\" names `{test}`, which was not discovered"
            );
        }
    }

    #[test]
    fn no_claim_is_both_structural_and_lint_enforced() {
        for (structural, _, _) in STRUCTURAL_ENFORCED {
            assert!(
                !LINT_ENFORCED.iter().any(|(lint, _)| lint == structural),
                "\"{structural}\" is declared twice with different enforcement"
            );
        }
    }

    #[test]
    fn every_structural_claim_cites_a_present_artifact() {
        for (claim, path, token) in STRUCTURAL_ENFORCED {
            let text = fs::read_to_string(repository_root().join(path))
                .unwrap_or_else(|error| panic!("read {path}: {error}"));
            assert!(
                text.contains(token),
                "structural claim \"{claim}\" cites `{path}` for `{token}`, which is absent"
            );
        }
    }

    #[test]
    fn every_source_searching_test_is_declared_a_lint() {
        let declared: BTreeSet<&str> = LINT_ENFORCED.iter().map(|(_, test)| *test).collect();
        let undeclared: Vec<_> = source_searching_tests(&repository_root())
            .into_iter()
            .filter(|test| !declared.contains(test.as_str()))
            .collect();
        assert!(
            undeclared.is_empty(),
            "undeclared substring assertions: {undeclared:?}"
        );
    }

    #[test]
    fn every_listed_adversary_has_a_control_and_evidence() {
        let model = threat_model(&repository_root()).expect("threat model");
        assert!(
            !model.adversaries.is_empty(),
            "{THREAT_MODEL} lists no adversaries; the check is inert"
        );
        let unanswered: Vec<_> = model
            .adversaries
            .iter()
            .filter(|adversary| adversary.coverage.is_empty())
            .map(|adversary| adversary.name.as_str())
            .collect();
        assert!(
            unanswered.is_empty(),
            "unanswered adversaries: {unanswered:?}"
        );
    }

    /// Coverage nests inside the adversary it answers, so an answer to an
    /// adversary the model does not list cannot be written down. What can still
    /// go wrong is a property nobody declared, which this catches.
    #[test]
    fn threat_coverage_names_only_declared_properties() {
        let model = threat_model(&repository_root()).expect("threat model");
        for adversary in &model.adversaries {
            for row in &adversary.coverage {
                assert!(
                    model.properties.contains(&row.property),
                    "{} claims property `{}`, which the model does not declare",
                    adversary.name,
                    row.property
                );
            }
        }
    }

    #[test]
    fn every_cited_evidence_exists() {
        let root = repository_root();
        let model = threat_model(&root).expect("threat model");
        let tests = rust_source_corpus(&root.join("crates"));
        let makefile = fs::read_to_string(root.join("Makefile")).expect("read Makefile");
        for adversary in &model.adversaries {
            for row in &adversary.coverage {
                let is_test = tests.contains(&format!("fn {}", row.evidence));
                let is_target = makefile.contains(&format!("\n{}:", row.evidence));
                assert!(
                    is_test || is_target,
                    "{}/{} cites `{}`, which is neither a test nor a make target",
                    adversary.name,
                    row.property,
                    row.evidence
                );
            }
        }
    }

    #[test]
    fn repository_inventory_is_current() {
        check(&repository_root()).unwrap();
    }
}
