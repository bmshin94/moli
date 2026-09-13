//! Legacy search-provider APIs are intentional no-ops in HTML and Blink.

use anyhow::{Result, anyhow};
use moli_webapi_declare::{WebApiFunctionTemplate, WebApiObject};

use crate::{util::throw_type_error, web_api_interfaces};

#[derive(WebApiObject)]
#[webapi(interface = web_api_interfaces::External)]
struct ExternalObjectDeclaration {}

#[derive(WebApiFunctionTemplate)]
#[webapi(interface = web_api_interfaces::External, enumerable)]
struct ExternalPrototypeDeclaration {
    #[webapi(method = "AddSearchProvider", length = 0, callback = external_noop_callback)]
    add_search_provider: (),
    #[webapi(method = "IsSearchProviderInstalled", length = 0, callback = external_noop_callback)]
    is_search_provider_installed: (),
}

pub(in crate::context_bootstrap) fn install_external_template_bindings<'s>(
    scope: &mut v8::PinScope<'s, '_, ()>,
    template: v8::Local<'s, v8::FunctionTemplate>,
    interface_name: &str,
) {
    if interface_name == "External" {
        ExternalPrototypeDeclaration::initialize_prototype_template(
            scope,
            template.prototype_template(scope),
        );
    }
}

pub(in crate::context_bootstrap) fn build_window_external<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Object>> {
    ExternalObjectDeclaration::new()
        .bind(scope)
        .map_err(|error| anyhow!("failed to bind External object: {error}"))
}

fn external_noop_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s, v8::Value>,
) {
    if !web_api_interfaces::External::is_instance(scope, args.this()) {
        throw_type_error(scope, "Illegal invocation");
        return;
    }
    // Neither operation has IDL arguments: extra values must not be coerced.
    rv.set_undefined();
}
