/// Enter JavaScript with the same native microtask nesting boundary used by
/// Blink's V8ScriptRunner. Page isolates use Explicit policy, so the renderer
/// still chooses when to checkpoint after returning from the outermost call.
pub(crate) fn run<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    execute: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let microtasks = std::pin::pin!(v8::MicrotasksScope::new(
        scope,
        v8::MicrotasksScopeType::RunMicrotasks,
    ));
    execute(microtasks.init())
}
