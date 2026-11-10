//! Erzeugt den Code für `#[tool]`.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, FnArg, ItemFn, Lit, LitStr, Meta, Pat, Type, parse::Parser};

struct Options {
    name: Option<String>,
    effect: syn::Ident,
    idempotent: bool,
    timeout_secs: u64,
}

fn parse_options(attr: TokenStream) -> syn::Result<Options> {
    let mut options = Options {
        name: None,
        effect: syn::Ident::new("Write", Span::call_site()),
        idempotent: false,
        timeout_secs: 60,
    };
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("idempotent") {
            options.idempotent = true;
        } else if meta.path.is_ident("effect") {
            let value: LitStr = meta.value()?.parse()?;
            let variant = match value.value().as_str() {
                "read_only" => "ReadOnly",
                "write" => "Write",
                "external" => "External",
                _ => {
                    return Err(syn::Error::new(
                        value.span(),
                        "erlaubt: \"read_only\", \"write\", \"external\"",
                    ));
                }
            };
            options.effect = syn::Ident::new(variant, value.span());
        } else if meta.path.is_ident("timeout") {
            let value: syn::LitInt = meta.value()?.parse()?;
            options.timeout_secs = value.base10_parse()?;
        } else if meta.path.is_ident("name") {
            let value: LitStr = meta.value()?.parse()?;
            options.name = Some(value.value());
        } else {
            return Err(
                meta.error("unbekanntes Attribut; erlaubt: effect, idempotent, timeout, name")
            );
        }
        Ok(())
    });
    parser.parse2(attr)?;
    Ok(options)
}

/// Der Doc-Kommentar der Funktion als ein Text.
fn description(function: &ItemFn) -> String {
    let mut lines = Vec::new();
    for attribute in &function.attrs {
        if let Meta::NameValue(pair) = &attribute.meta
            && pair.path.is_ident("doc")
            && let Expr::Lit(ExprLit {
                lit: Lit::Str(text),
                ..
            }) = &pair.value
        {
            lines.push(text.value().trim().to_owned());
        }
    }
    lines.join("\n").trim().to_owned()
}

fn is_option(ty: &Type) -> bool {
    matches!(ty, Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == "Option"))
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let options = parse_options(attr)?;
    let function: ItemFn = syn::parse2(item)?;
    let signature = &function.sig;
    if signature.asyncness.is_none() {
        return Err(syn::Error::new(
            signature.fn_token.span(),
            "#[tool] braucht eine `async fn`",
        ));
    }
    if !signature.generics.params.is_empty() {
        return Err(syn::Error::new(
            signature.generics.span(),
            "#[tool] unterstützt keine generischen Funktionen",
        ));
    }

    let mut takes_ctx = false;
    let mut names = Vec::new();
    let mut types = Vec::new();
    for (index, input) in signature.inputs.iter().enumerate() {
        let FnArg::Typed(argument) = input else {
            return Err(syn::Error::new(
                input.span(),
                "#[tool] unterstützt kein `self`",
            ));
        };
        if index == 0 && matches!(*argument.ty, Type::Reference(_)) {
            takes_ctx = true;
            continue;
        }
        let Pat::Ident(pattern) = &*argument.pat else {
            return Err(syn::Error::new(
                argument.pat.span(),
                "Parameter müssen einfache Namen sein",
            ));
        };
        names.push(pattern.ident.clone());
        types.push((*argument.ty).clone());
    }

    let ident = &signature.ident;
    let visibility = &function.vis;
    let tool_name = options.name.unwrap_or_else(|| ident.to_string());
    let text = description(&function);
    if text.is_empty() {
        return Err(syn::Error::new(
            ident.span(),
            "#[tool] braucht einen Doc-Kommentar als Beschreibung",
        ));
    }
    let effect = &options.effect;
    let idempotent = options.idempotent;
    let timeout_secs = options.timeout_secs;
    let keys: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    let required: Vec<bool> = types.iter().map(|ty| !is_option(ty)).collect();
    let ctx_argument = if takes_ctx { quote!(__cx,) } else { quote!() };
    let mut inner = function.clone();
    inner.sig.ident = syn::Ident::new("__run", ident.span());
    inner.vis = syn::Visibility::Inherited;
    inner
        .attrs
        .retain(|attribute| !attribute.path().is_ident("doc"));
    let docs = function
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"));

    Ok(quote! {
        #(#docs)*
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, Debug, Default)]
        #visibility struct #ident;

        impl #ident {
            #inner
        }

        impl ::agentkit_core::Tool for #ident {
            fn spec(&self) -> &::agentkit_core::ToolSpec {
                static SPEC: ::std::sync::OnceLock<::agentkit_core::ToolSpec> = ::std::sync::OnceLock::new();
                SPEC.get_or_init(|| {
                    let schema = ::agentkit_core::schema::object_schema(::std::vec![
                        #((#keys, ::agentkit_core::__private::schema_of::<#types>(), #required),)*
                    ]);
                    ::agentkit_core::ToolSpec::new(#tool_name, #text, schema)
                        .with_effect(::agentkit_core::Effect::#effect)
                        .with_idempotent(#idempotent)
                        .with_timeout(::std::time::Duration::from_secs(#timeout_secs))
                })
            }

            fn call<'life0, 'life1, 'async_trait>(
                &'life0 self,
                args: ::agentkit_core::__private::serde_json::Value,
                __cx: &'life1 ::agentkit_core::ToolCtx,
            ) -> ::std::pin::Pin<::std::boxed::Box<
                dyn ::std::future::Future<
                        Output = ::std::result::Result<::agentkit_core::ToolOutput, ::agentkit_core::ToolError>,
                    > + ::std::marker::Send
                    + 'async_trait,
            >>
            where
                'life0: 'async_trait,
                'life1: 'async_trait,
                Self: 'async_trait,
            {
                ::std::boxed::Box::pin(async move {
                    let _ = __cx;
                    #[allow(unused_mut)]
                    let mut __args = ::agentkit_core::__private::args_object(args)?;
                    #(let #names: #types = ::agentkit_core::__private::arg(&mut __args, #keys)?;)*
                    Self::__run(#ctx_argument #(#names),*).await.map(::std::convert::Into::into)
                })
            }
        }
    })
}
