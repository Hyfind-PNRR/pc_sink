# Project Instructions

## Workflow

This project is developed through GitHub issues. When assigned an issue:

1. Read the issue carefully, including any linked issues or referenced context.
2. Ask clarifying questions as comments on the issue **before** starting if anything is ambiguous.
3. Create a feature branch named `issue-<number>-<short-slug>`.
4. Implement the work according to the guidelines below.
5. Open a PR that references the issue (`Closes #N`), with a brief summary of what changed and why.

Do not start implementation if the acceptance criteria are unclear. A wrong implementation is worse than a delayed one.

---

## Language & Toolchain

- **Rust stable** (track the current stable release; no nightly unless explicitly required).
- Use `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` before considering any task done. All three must pass clean.
- `Cargo.toml` must declare exact dependency versions (`=x.y.z`) for direct dependencies unless a range is explicitly justified.

---

## Idiomatic Rust

Write code that a senior Rust engineer would be proud to review. Concretely:

- **Ownership first.** Prefer owned types when the cost is negligible; borrow when ownership transfer is unnecessary.
- **No unnecessary clones.** Every `.clone()` requires a comment explaining why borrowing was not sufficient.
- **Use iterators and combinators** (`map`, `filter`, `fold`, `flat_map`, `collect`, etc.) rather than explicit `for` loops where it improves clarity. Never mutate inside an iterator chain.
- **Error handling via `Result` and `?`.** Never use `.unwrap()` or `.expect()` outside of tests or `main` startup assertions. Propagate errors; do not swallow them. Use `thiserror` for library errors and `anyhow` for application-level errors (or whichever the project already uses — stay consistent).
- **No `panic!` in library code.** Panics are only acceptable in `main`, CLI argument parsing, or clearly documented invariants.
- Prefer `Option` and `Result` methods (`.map()`, `.and_then()`, `.ok_or()`) over `match` when the intent is a simple transformation.

---

## Type System

Leverage the type system to make invalid states unrepresentable:

- **Newtype pattern** for domain primitives (e.g., `struct UserId(u64)` rather than bare `u64`). Implement `Display`, `Debug`, and relevant `From`/`Into` conversions.
- **Enum over bool flags.** Replace `bool` parameters and fields that encode semantic state with purpose-named enums (e.g., `enum Visibility { Public, Private }` not `is_public: bool`).
- **Phantom types and type-state patterns** where a value's validity depends on prior operations (e.g., `Builder<Unvalidated>` → `Builder<Validated>`).
- **Sealed traits** to prevent external implementations where the set of implementors must be controlled.
- **Non-empty collections** (`Vec` with a known-non-empty invariant) should use a wrapper type, not a raw `Vec` with a runtime check scattered everywhere.
- Avoid `any`, downcasting (`as Any`), and `unsafe` unless there is no alternative; every `unsafe` block must have a `// SAFETY:` comment.

---

## Functional Style

- Functions should be **pure where possible**: same inputs → same outputs, no hidden state, no side effects.
- **Separate I/O from logic.** Core business logic must not perform I/O (filesystem, network, time, randomness). Pass dependencies in; do not reach for them.
- Prefer **small, composable functions** over large ones. A function that does more than one thing at the abstraction level of its name should be split.
- Avoid mutable state. Prefer returning new values over mutating existing ones. When mutation is necessary, scope it tightly.
- **Data pipelines** (parsing → transformation → output) should be modeled as a chain of pure transformations, with I/O only at the entry and exit points.

---

## Clean Code

- **Names are documentation.** Variables, functions, types, and modules must be named for what they represent, not how they are implemented. Abbreviations are forbidden unless they are domain-standard (e.g., `id`, `url`, `cfg`).
- **One responsibility per module and function.** If you need to write "and" to describe what a function does, split it.
- **No magic numbers or strings.** Use named constants or enum variants.
- **Keep functions short.** As a guideline, if a function body does not fit comfortably on one screen, it is probably doing too much.
- **Avoid deep nesting.** Use early returns, `?`, and helper functions to flatten control flow.
- **Dead code is deleted**, not commented out.

---

## Documentation

Document with `///` (rustdoc) following these rules:

- Every `pub` item (struct, enum, trait, function, method, module) **must** have a rustdoc comment unless its name is completely self-explanatory in context.
- The first line is a single sentence in indicative mood ("Returns the user by ID." not "Return the user by ID.").
- Add `# Errors`, `# Panics`, and `# Examples` sections where applicable.
- Do **not** document private implementation details unless the code is genuinely non-obvious — prefer making the code clearer instead.
- Module-level `//!` comments explain the purpose and scope of the module.

Example:

```rust
/// Returns the normalized form of a tag name.
///
/// Lowercases and trims whitespace. Returns `None` if the result would be empty.
///
/// # Examples
///
/// ```
/// assert_eq!(normalize_tag("  Rust "), Some("rust".to_string()));
/// assert_eq!(normalize_tag("   "), None);
/// ```
pub fn normalize_tag(raw: &str) -> Option<String> { ... }
```

---

## Testing

- **Every public function has at least one unit test** covering the happy path.
- **Edge cases and error paths** must be tested explicitly: empty inputs, boundary values, invalid data, error propagation.
- Tests live in an inline `#[cfg(test)] mod tests { ... }` block in the same file as the code under test, **except** for integration tests which live in `tests/`.
- Test names follow `snake_case` and describe the scenario and expected outcome: `returns_none_when_input_is_empty`, not `test1`.
- Use `proptest` or `quickcheck` for property-based tests on non-trivial parsing or transformation logic.
- No test should touch the real filesystem, network, or clock. Use dependency injection or traits to abstract I/O so tests can supply fakes.
- `#[should_panic]` is a last resort. Prefer testing error returns with `assert!(result.is_err())`.

---

## What Not To Do

- Do not introduce new dependencies without noting the justification in the PR description.
- Do not change unrelated code in the same PR.
- Do not leave `TODO` or `FIXME` comments without an associated GitHub issue number.
- Do not silence Clippy warnings with `#[allow(...)]` without a comment explaining why the lint is inapplicable.
- Do not merge to `main` without a passing CI run.