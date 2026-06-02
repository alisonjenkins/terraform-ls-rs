# Completion Deep-Dive Backlog

Source: multi-agent completion deep-dive (7 survey lenses → per-finding assess/verify → synthesize), 2026-06-02.
Workflow: `completion-deep-dive`. 48 agents, ~2.5M tokens. Bugs adversarially refuted; features/improvements scored for value, effort, and non-duplication.

**Counts (after dedup): 12 bugs (3 high, 8 medium, 1 low), 7 missing features (6 medium, 1 low), 13 improvements.** Effort = rough size (S/M/L); confidence from the assessing agent. Legend: `[ ]` open, `[x]` done, `[~]` won't fix.

---

## Bugs

- [x] **LSP Position.character treated as bytes, not UTF-16 — wrong context + panic on non-ASCII lines** (high, effort M, confidence high) — crates/tfls-parser/src/position.rs:56 (via completion.rs:77); crates/tfls-lsp/src/handlers/completion.rs:2840 (compute_index_replace_range); capabilities.rs:29
  `lsp_position_to_byte_offset` adds `pos.character` (UTF-16 code units per spec; no positionEncoding negotiated) as raw bytes; on any line with multibyte text before the cursor the slice is wrong (misclassification) and can land mid-codepoint, panicking `&source[..byte_offset]` in classify_context — a reachable runtime panic that aborts completion. The same byte-as-UTF16 confusion in compute_index_replace_range (the one completion path emitting a textEdit) misaligns the replace Range, corrupting the buffer on accept.
  **Proposal:** Either advertise `positionEncoding=utf-8` in the initialize handshake (and honor client negotiation), or convert UTF-16 columns to byte offsets via ropey (`utf16_cu_to_char`/`char_to_byte`) for all `lsp_position_to_byte_offset` callers; in compute_index_replace_range do rfind/consume math in bytes then convert returned columns back to UTF-16. Add a multibyte-before-cursor test asserting correct columns.

- [x] **Comments are not masked: any brace/quote in a comment corrupts the classifier** (high, effort M, confidence high) — crates/tfls-core/src/completion.rs:1061 (ignored_brace_positions), :1254 (enclosing_block_context), :1519 (is_top_level), :573 (expression_context), :356 (unterminated_string_open)
  The classifier has no comment handling; `#`, `//`, `/* */` content is read as live code. `resource "x" "w" {\n # } closing?\n |` classifies as TopLevel; `value = 1 # see var.|` → VariableRef; a stray `"` in a comment unbalances string tracking for everything after. Wrong/irrelevant completions constantly in real comment-laden files.
  **Proposal:** Extend ignored_brace_positions into one source-state scanner that masks comment runs (`#`/`//` to end-of-line, `/* */` to closing) when not in a string; rewrite is_top_level and expression_context (and classify_block_header_from) to consume the same mask so comment braces no longer corrupt depth.

- [x] **Version completions never set a textEdit replace range — accepting mid-typing garbles constraints** (high, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:980-988, 1016-1023, 1134-1144, 1162-1169, 1232-1244
  Provider/module/tool version items carry only `insert_text` (full version) with no `text_edit`/`filter_text`. Typing `version = "5.9|"` routes to InsideVersion and accepting `5.94.0` yields `version = "5.95.94.0"` on clients that don't strip the typed prefix. `.` is a trigger char so this is the normal flow.
  **Proposal:** Compute a UTF-16 replace range over the typed partial (thread `string_open`/partial from string_value_context) and emit `CompletionTextEdit::Edit` on every version item, mirroring index_key_items; at minimum set `filter_text` to the bare version.

- [x] **Function/expression completion fires inside plain string literals** (medium, effort M, confidence high) — crates/tfls-core/src/completion.rs:573 (expression_context), :251/:300 (string_value_context)
  For an unrecognized string attribute, string_value_context returns None and expression_context (string-unaware) offers FunctionCall/AttributeValue inside opaque string content: `ami = "foo(|` → FunctionCall, `ami = "a = |` → AttributeValue. Spurious menus while typing free text.
  **Proposal:** In classify_context/expression_context, detect via unterminated_string_open that the cursor is inside a string and not inside a `${`/`%{` interpolation opened after the quote; return Unknown (or a StringLiteral context) instead of running expression_context.

- [x] **Attribute-value context lost inside nested blocks (root_block_device, ingress, …)** (medium, effort M, confidence high) — crates/tfls-core/src/completion.rs:627 (attribute_value_context), :656 (classify_block_header_from)
  classify_block_header_from only inspects the innermost `{`; if it's a nested block it returns None, so `attr = |` inside e.g. `root_block_device {` degrades to FunctionCall, losing variables, locals, and schema-aware resource/data reference suggestions.
  **Proposal:** When the innermost block isn't resource/data, keep walking outward to find the owning resource/data type (reuse enclosing_block_context/BuiltinNestedBody), carry the nested path, and emit AttributeValue with the resolved resource_type even several block levels deep.

- [x] **Namespace fast-paths shadow user identifiers named path/count/each/terraform** (medium, effort S, confidence high) — crates/tfls-core/src/completion.rs:721 (reference_prefix_context), lines 731-762
  Raw `ends_with("count.")`/`"path."`/`"each."`/`"terraform.")` matches fire before the multi-segment arm, so `var.count.|` → CountRef, `local.terraform.|` → TerraformNamespaceRef. Valid object-typed vars/locals get the wrong namespace's completions instead of their fields.
  **Proposal:** Drop the ends_with fast-paths and route through traversal_segments_reverse, matching `["count"]`/`["path"]`/etc. as exact single-segment slices (multi-segment already handles `["var","count"]` correctly).

- [x] **index_key_items emits text_edit Range using byte offsets, not UTF-16** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:2840-2871 (compute_index_replace_range)
  `pos.character` is used as a byte index into the line and emitted Range columns are byte offsets; a multibyte char before `[` (e.g. `var.régions["…"]`) misaligns the only offset-sensitive reference path, overwriting the wrong span on accept. (Shares root with the position.rs UTF-16 bug above.)
  **Proposal:** Convert pos.character (UTF-16) to a byte index over the line, do rfind/consume in bytes, then convert bracket_col and (byte_col+consumed) back to UTF-16 code-unit columns for the Range. Add a multibyte-before-`[` test.

- [x] **Nested blocks with max_items=1 (List/Set) re-suggested after one is present** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1398-1414
  resource_body_items suppresses an already-present block only for NestingMode::Single, but providers encode max-one blocks as List/Set + max_items=1 (root_block_device, etc.). They keep appearing; adding a second is a hard Terraform error (which the server's own diagnostics then flag). max_items is never read in completion.rs.
  **Proposal:** Track present-block counts (HashMap<String,usize>) in BodyFilter and suppress when `nb.max_items != 0 && present_count >= nb.max_items` regardless of nesting_mode; keep Single as the max_items=1 special case.

- [x] **`~> MAJOR.MINOR` operator snippets offered after an operator already typed → double operator** (medium, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1031-1041, 1177-1187, 1251-1261
  The AfterOperator/InsideVersion branch's `*_from_registry`/`*_from_github` builders unconditionally append `~> MM` items; reached when the user already typed an operator (`version = ">= |"`), so selecting `~> 4.71` yields invalid `">= ~> 4.71"`.
  **Proposal:** Pass the CursorSlot (or `at_operator` bool) into the three builders and emit operator-prefixed entries only in the AtOperator/Trailing slot; emit bare versions in the after-operator slot.

- [x] **Provider-defined functions leak into plain function-call completion with uninvokable insert text** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1845-1871 (function_name_items)
  function_name_items iterates the shared functions DashMap unfiltered, emitting items labelled `provider::hashicorp::aws::arn_parse` whose insert_text is the raw registry-namespaced key — invalid HCL (Terraform needs the local provider name). Pollutes the builtin menu in TF 1.8+ workspaces; surfaces in both FunctionCall and reference-expression paths.
  **Proposal:** In function_name_items, skip keys starting with `provider::` (filter_map); provider functions are already surfaced correctly via the ProviderFunctionNamespace/Name contexts.

- [x] **resource_scaffold_snippet hard-codes quoted placeholders → invalid HCL for numeric/bool/list/object required attrs** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:422
  The top-level resource/data scaffold fills every required attr as `name = "${N}"` regardless of type (discards the AttributeSchema), so a required number/bool/list inserts a quoted string and immediately errors. nested_block_scaffold_snippet already does this correctly.
  **Proposal:** Capture the AttributeSchema in the loop and route through classify_schema_type: string → `"${n}"`, sequence → `[${n}]`, mapping → `{ ${n} }`, scalar → `${n}` (as nested_block_scaffold_snippet does).

- [x] **`cursor_slot` misclassifies a half-typed `~>` operator as a version token** (low, effort S, confidence high) — crates/tfls-core/src/version_constraint.rs:322-339
  detect_operator("~") returns (Eq, 0), so input `~` becomes InsideVersion{partial:"~"} and the menu switches from operators to exact versions whose labels don't start with `~`, collapsing to empty over the most common operator's first keystroke.
  **Proposal:** In cursor_slot, when the trimmed piece is a partial operator prefix (`~`, `!`, or a leading run of operator chars not yet a complete token with no version after it), return AtOperator so operator items keep showing until the operator is complete.

## Missing features

- [x] **No completion inside .tfvars files (variable-name keys + value completion)** (medium, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:66; crates/tfls-core/src/completion.rs:241
  completion() never checks the file extension; a partial key in tfvars classifies as TopLevel and offers invalid block snippets (resource/variable/…). terraform-ls completes declared variable names as keys (+ value completion). tfvars are already discovered/indexed.
  **Proposal:** Add a `TfvarsKey{partial}`/`TfvarsValue{name}` context (or a uri-extension pre-check); for keys reuse module_symbol_items over sibling `variable` decls emitting `name = ${1}` snippets; for values reuse attribute-enum logic where the variable type's shape is known (best-effort).

- [x] **Top-level blocks moved/import/removed/check never offered (keyword + body)** (medium, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:39 (TOP_LEVEL_SNIPPETS); crates/tfls-core/src/completion.rs:1468 (classify_block_header)
  TOP_LEVEL_SNIPPETS lists only 8 classic blocks; `moved` (TF1.1), `import`/`check` (1.5), `removed` (1.7) are never suggested and their bodies get no attr completion (classify_block_header doesn't recognize the keywords). The codebase's own deprecation auto-fix emits `moved{}` yet can't complete it.
  **Proposal:** Add snippet entries for import/moved/removed/check; add BuiltinSchema tables (import: to/id/provider/for_each; moved: from/to; removed: from + lifecycle; check: assert/data sub-blocks) and wire keywords into classify_block_header + resolve_nested_schema (and update schema-detail/hardcoded-list test invariants).

- [x] **No body completion inside connection {} or provisioner "…" {} blocks** (medium, effort M, confidence high) — crates/tfls-core/src/builtin_blocks.rs:668-689 (RESOURCE_ROOT_SCHEMA); crates/tfls-lsp/src/handlers/completion.rs:1334-1361 (resource_body_items)
  connection/provisioner are offered as meta-block snippets and `self.` is classified inside them, but inside their bodies resource_body_items descends the provider schema (no such block type) and returns nothing — no completion for type/host/user/private_key/… or command/working_dir/when/on_failure/interpreter.
  **Proposal:** Add CONNECTION_BLOCK + per-provisioner schemas (local-exec/remote-exec/file) as children of RESOURCE_ROOT_SCHEMA, and route `connection`/`provisioner` paths through resolve_nested_schema like `lifecycle` does; thread the provisioner label through BlockStep (or union-schema fallback for unknown labels).

- [ ] **`ignore_changes`/`replace_triggered_by`/`depends_on` lists offer no attribute/address completion** (medium, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1419-1430; crates/tfls-core/src/completion.rs:573 (expression_context)
  Inside `lifecycle { ignore_changes = [|] }` the cursor falls to FunctionCall, offering only function names — actively wrong, since ignore_changes takes bare attribute identifiers. replace_triggered_by/depends_on get no resource/data/module address references.
  **Proposal:** Intercept the `[`-interior of these attrs before expression_context and emit new contexts: IgnoreChangesList{resource_type} → enclosing resource's bare attribute names (via resource_attr_items) plus `all`; replace_triggered_by/depends_on → resource/data/module addresses via a new StateStore address enumerator.

- [ ] **each.value.<field> drill-down never offered despite available shape data** (medium, effort M, confidence high) — crates/tfls-core/src/completion.rs:740-744,772-794; handler each_namespace_items
  `each.value.|` strips to `["each","value"]` and falls to Unknown because the `[t,n]` arm is guarded by `!is_builtin_prefix(t)` and `each` is builtin; yet for_each element shapes are already inferred/stored. One of the most common reference patterns yields nothing.
  **Proposal:** Add CompletionContext::EachAttr{path} classified from `["each","value", rest..]`; resolve the enclosing block's for_each address, unwrap one level (Map→value / Set/List→element / Object→field) from the stored collection shape, and walk_shape along rest to offer object fields (mirror index_key_items). Cover data/module for_each variants.

- [x] **Resource/data attribute reference completion omits nested block names (block_types)** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:2523-2553 (resource_attr_items)
  After `<resource>.<name>.` (ResourceAttr/DataSourceAttr/SelfRef) only schema.block.attributes are offered; nested blocks (root_block_device, network_interface, subnet_mapping, …) are valid referenceable targets but never suggested. Same gap for `self.<nested_block>`. (Subsumes the low-severity duplicate of this finding.)
  **Proposal:** After collecting attribute items, also iterate `schema.block.block_types` and push one FIELD/STRUCT item per nested-block name (detail "nested block"), optionally hinting an index from nesting_mode; filter config-only blocks (lifecycle/timeouts) that aren't valid reference targets.

- [ ] **for-expression loop variables misclassified as resource-type references** (low, effort L, confidence high) — crates/tfls-core/src/completion.rs:721 (reference_prefix_context), match arm `[t] => ResourceRef` line 786
  `[for x in var.list : x.|]` classifies as ResourceRef{resource_type:"x"} since `x` isn't a builtin prefix; downstream this almost always yields an empty menu, so for-binding field completion never works.
  **Proposal:** Scan the enclosing bracket/brace group for `for <ident>[, <ident2>] in` binders before the cursor; treat those names as locally-bound, suppress the ResourceRef misclassification, and (ideally) infer the iterated collection's element shape to offer fields (new ForBindingRef).

## Improvements

- [ ] **Resource/data type completion clones the full provider schema per keystroke and never sets isIncomplete** (medium, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:265 (resource_type_items) / 401 (resource_scaffold_snippet); dispatcher :247
  ResourceType/DataSourceType calls resource_scaffold_snippet for each of ~1000-1400 types, each doing `resource_schema(...).cloned()` on the owned Schema — thousands of full clones + snippet bodies per keystroke, returned as a complete Array. Real cost cliff on large providers.
  **Proposal:** Build bare-label items first; either lazily attach the scaffold via completionItem/resolve (enable resolve_provider) or return `List{is_incomplete:true}` with a capped page. At minimum add a borrow-based schema accessor so the scaffold reads required attrs without cloning Schema.

- [x] **Schema body completion does not surface required attributes above optional ones** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1375-1393,1432; builtin_body_items :558
  Attribute items have no sort_text and are flat-sorted by label, intermixing required and optional; users hunt for the must-fill fields (e.g. aws_instance.ami) among dozens of optionals.
  **Proposal:** Set required-first sort_text buckets (`0_{name}` required, `1_{name}` optional) in both builders; give nested-block/meta items their own later buckets so unprefixed labels don't sort ahead.

- [x] **After-operator/inside-version completions return the full unfiltered version list every keystroke** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:996-1043, 1148-1189, 1213-1262 (and the InsideVersion arms discarding `partial`)
  The registry builders ignore the typed `partial`, emitting every version (1000+ for aws) each keystroke; no filter_text. Wasteful serialization, and clients without label-prefix matching show the unfiltered menu. (Merged with the duplicate "lists every version" finding.)
  **Proposal:** Thread `partial` into the three builders and pre-filter `versions` to `starts_with(partial)` (preserving descending order; keep matching `~> MM` templates), falling back to the full list when empty; optionally cap to top-N newest plus matches.

- [x] **Top-level block completion menu offers `dynamic`/blocks past max_items** (medium, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1399-1405 (resource_body_items)
  resource_body_items suppresses re-offered nested blocks only on NestingMode::Single, ignoring max_items, so a List/Set + max_items=1 block already present is still offered as repeatable. (Keep only this part; drop the dynamic-suppression sub-idea — a `dynamic` over a 0/1-element collection is a valid idiom the validator deliberately exempts.)
  **Proposal:** Extend compute_body_filter to count occurrences per block name and suppress when `present_count >= max_items`; leave `dynamic` always offered.

- [x] **`provider`/`required_version` value completion preselects higher-major pre-releases as "latest"** (low, effort S, confidence high) — crates/tfls-provider-protocol/src/registry_versions.rs:738-767; crates/tfls-lsp/src/handlers/completion.rs:919-937,1089
  prefilled_provider_version_items/prefilled_tool_version_items take `versions.first()` as latest; since semver_key ranks major-dominant, `6.0.0-beta1` outranks stable `5.99.0`, so a beta gets preselected and pinned in the `~> 6.0`/`>= …` defaults. (Module path is unaffected; no user-facing toggle needed.)
  **Proposal:** Pick the latest STABLE version (`find(|v| !v.version.contains('-')).or_else(first)`) for the preselected `0000_latest` default and the operator flavours, matching cached_latest_version's existing policy; keep pre-releases in the full list.

- [x] **Deprecated attributes/blocks offered with no de-prioritization** (low, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1363-1393 (resource_body_items), 1821-1843 (attribute_detail)
  AttributeSchema/BlockSchema.deprecated only appends "deprecated" to detail; items still appear at equal priority with no CompletionItemTag::DEPRECATED and no sort penalty, steering users into deprecated fields.
  **Proposal:** When deprecated, set `tags: Some(vec![CompletionItemTag::DEPRECATED])` and a lower sort bucket (add CompletionItemTag to the lsp_types import); do not skip them (legacy configs still need to complete pre-existing deprecated fields).

- [x] **`dynamic` content menu does not respect target block's max_items** (low, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:1644-1681 (dynamic_body_items / resource_body_items nested-block loop)
  Narrowed scope: the resource_body_items nested-block loop offers blocks usable via `dynamic` without max_items awareness (same root as the max_items bug). Menu polish; diagnostics already flag exceeding max_items.
  **Proposal:** Once count-aware max_items suppression exists, apply it to the nested-block names offered as `dynamic` targets so the menu matches what Terraform accepts; do not suppress the `dynamic` keyword itself.

- [x] **attribute_value_items mixes sort_text'd refs with un-sorted function items, scrambling order** (low, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:2278 (esp. :2360)
  Enum/resource/data/var/local items get `{NNNN}_` sort_text, then appended function_name_items carry none, so alphabetical function labels interleave with the prefixed refs (LSP falls back to label sort), losing the "refs first, functions fallback" ordering.
  **Proposal:** At the line-2360 append site, re-stamp the function items with a high sort bucket (e.g. `9999_`+name) continuing sort_index — not inside function_name_items (which is reused standalone for FunctionCall where alphabetical is correct).

- [x] **No `for`/ternary expression scaffolding snippets in expression context** (low, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:182 (FunctionCall → function_name_items)
  Expression positions return only function names; no list-for/map-for comprehension or conditional snippets that users reach for constantly. (Drop `for_each`/`dynamic` from scope — those are meta-arg/block, already handled.)
  **Proposal:** Prepend a static SNIPPET set when ctx==FunctionCall with `00_` sort prefixes: `[for ${1:item} in ${2:list} : ${3:item}]`, `{ for ${1:k}, ${2:v} in ${3:map} : ${1:k} => ${2:v} }`, `${1:cond} ? ${2:a} : ${3:b}` (mirror TOP_LEVEL_SNIPPETS).

- [x] **Expression `=` values in output/locals/module blocks only get functions+vars, not full references** (medium, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:197 (OutputBlockBody) / 178 (AttributeValue); crates/tfls-core/src/completion.rs:670
  classify_block_header_from requires resource/data, so bare `output "x" { value = | }` (and locals/module-input `=`) falls to FunctionCall and offers only functions — no reference roots. (Once `var.`/`local.`/`module.` is typed, the rich path already fires; the gap is the bare expression-start position.)
  **Proposal:** Add a CompletionContext returned by expression_context for non-resource/data enclosing blocks that offers reference ROOT keywords (var/local/module/each/path, resource type names, data) plus functions, reusing existing enumeration helpers; preserve the resource/data AttributeValue path.

- [ ] **No de-duplication / single-flight for concurrent registry fetches on rapid keystrokes** (low, effort M, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:866-893, 1050-1069; crates/tfls-provider-protocol/src/registry_versions.rs:96-130
  Each completion builds a fresh reqwest client (dropped at fn end, losing pool reuse) and there's no in-flight coalescing, so cold-cache rapid typing fires overlapping full fan-out fetches (largely mitigated by existing did_open/initialize prefetch and the disk cache).
  **Proposal:** Hold a shared reqwest::Client on Backend; add an in-flight coalescer (DashMap<(ns,name), Shared<Arc<…>>> or a Mutex-guarded request map) gated by is_provider_cached so concurrent completions for the same provider await one fetch.

- [x] **Every completion request does a full rope.to_string() before classifying** (low, effort S, confidence high) — crates/tfls-lsp/src/handlers/completion.rs:84
  completion() materializes the whole document each keystroke though classify_context only reads `&source[..byte_offset]`.
  **Proposal:** Materialize only the prefix (`rope.byte_slice(..offset).to_string()`) and pass it to classify_context (offset == len); also materialize a small post-cursor slice (to end-of-line) for label_closed_after, which reads `&text[offset..]`.

- [ ] **Missing `[`/`:`/`$` trigger characters for index/provider-function/interpolation contexts; `"` fires on name labels** (low, effort M, confidence high) — crates/tfls-lsp/src/capabilities.rs:30
  trigger_characters are only `.` and `"`; IndexKeyRef (`["`), provider-function (`provider::`), and interpolation (`${`) contexts never auto-pop, and `"` fires a wasted classify+full rope.to_string() on every quote (including resource name labels).
  **Proposal:** Add `[` (cleanest) and consider `:`/`$`; to keep cost sane, bail classify cheaply before the rope.to_string() on common `:`/`$` keystrokes that won't produce items. Add tests routing each new trigger to its intended context.
