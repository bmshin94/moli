use super::super::{document_runtime::DomHandle, native_bridge::JsContextHost};
use super::construction_failure::ConstructionFailure;
use super::construction_invocation::{
    CustomElementConstructorInvocation, invoke_custom_element_constructor,
};
use super::construction_result::{
    FailedExistingConstructionPrototype, finalize_wrapper_custom_element_prototype,
    synchronize_custom_element_prototype_between_wrappers,
};
use super::existing_upgrade_completion::{
    complete_existing_custom_element_upgrade, enqueue_existing_upgrade_callbacks,
    observed_attributes_with_current_values,
};
use super::existing_upgrade_failure::fail_existing_custom_element_construction;
use super::existing_upgrade_reentry::already_constructed_reentry_consumed_pending_handle;
use super::reactions::enter_upgrade_dynamic_markup_insertion;

pub(super) fn upgrade_existing_custom_element_with_constructor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    host_ptr: *mut JsContextHost,
    handle: DomHandle,
    wrapper: v8::Local<'s, v8::Object>,
    constructor: v8::Local<'s, v8::Function>,
    definition_name: &str,
    failure_prototype: FailedExistingConstructionPrototype,
) -> bool {
    let initial_attributes =
        observed_attributes_with_current_values(host_ptr, handle, definition_name);
    enqueue_existing_upgrade_callbacks(scope, host_ptr, handle, initial_attributes);
    unsafe { &mut *host_ptr }
        .custom_elements_mut_for_node_handle(handle)
        .begin_construction(scope, constructor, wrapper, handle);
    let _dynamic_markup = enter_upgrade_dynamic_markup_insertion(host_ptr, handle);
    let invocation = invoke_custom_element_constructor(scope, host_ptr, constructor);
    let consumed_pending_wrapper = unsafe { &*host_ptr }
        .custom_elements_for_node_handle(handle)
        .is_some_and(|store| store.pending_construction_is_already_constructed(handle));
    match invocation {
        CustomElementConstructorInvocation::Created(created) => {
            let created = v8::Local::new(scope, &created);
            if !validate_existing_custom_element_upgrade_result(wrapper, created) {
                fail_existing_custom_element_construction(
                    scope,
                    host_ptr,
                    handle,
                    constructor,
                    ConstructionFailure::TypeError(
                        "Custom element constructor returned a different element during upgrade",
                    ),
                    failure_prototype,
                );
                if consumed_pending_wrapper {
                    finalize_wrapper_custom_element_prototype(scope, wrapper, constructor);
                }
                return true;
            }
            finalize_wrapper_custom_element_prototype(scope, wrapper, constructor);
            complete_existing_custom_element_upgrade(scope, host_ptr, handle, definition_name);
            true
        }
        CustomElementConstructorInvocation::Exception(exception) => {
            if consumed_pending_wrapper {
                finalize_wrapper_custom_element_prototype(scope, wrapper, constructor);
            }
            if consumed_pending_wrapper
                && let Some(canonical) = unsafe { &mut *host_ptr }
                    .native_bridge_mut()
                    .wrap_handle(scope, host_ptr, handle)
            {
                if canonical.strict_equals(wrapper.into()) {
                    synchronize_custom_element_prototype_between_wrappers(
                        scope, wrapper, canonical,
                    );
                } else {
                    super::construction_result::set_wrapper_custom_element_constructor_prototype(
                        scope,
                        canonical,
                        constructor,
                    );
                }
            }
            let exception = v8::Local::new(scope, &exception);
            if already_constructed_reentry_consumed_pending_handle(
                scope,
                host_ptr,
                handle,
                exception,
                constructor,
                definition_name,
            ) {
                complete_existing_custom_element_upgrade(scope, host_ptr, handle, definition_name);
                return true;
            }
            fail_existing_custom_element_construction(
                scope,
                host_ptr,
                handle,
                constructor,
                ConstructionFailure::Exception(exception),
                failure_prototype,
            );
            if consumed_pending_wrapper {
                finalize_wrapper_custom_element_prototype(scope, wrapper, constructor);
            }
            true
        }
        CustomElementConstructorInvocation::Empty => {
            unsafe { &mut *host_ptr }
                .custom_elements_mut_for_node_handle(handle)
                .discard_pending_construction(handle);
            unsafe { &mut *host_ptr }
                .custom_element_reactions_mut()
                .clear_reactions(handle);
            false
        }
    }
}

fn validate_existing_custom_element_upgrade_result<'s>(
    expected_wrapper: v8::Local<'s, v8::Object>,
    created: v8::Local<'s, v8::Object>,
) -> bool {
    created.strict_equals(expected_wrapper.into())
}
