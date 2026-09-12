use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, ItemFn, ItemStruct, Pat, parse_macro_input};

/// Export a zero-argument SQL function as `loom_schema() -> u64` in the core
/// guest ABI. The result is a packed pointer/length to a Loom CBOR envelope.
#[proc_macro_attribute]
pub fn schema(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "schema takes no attributes")
            .to_compile_error().into();
    }
    let function = parse_macro_input!(item as ItemFn);
    let signature = &function.sig;
    let valid_return = matches!(&signature.output,
        syn::ReturnType::Type(_, ty) if matches!(ty.as_ref(), syn::Type::Reference(reference)
            if reference.mutability.is_none()
                && reference.lifetime.as_ref().is_some_and(|lifetime| lifetime.ident == "static")
                && matches!(reference.elem.as_ref(), syn::Type::Path(path) if path.path.is_ident("str"))));
    if !signature.inputs.is_empty() || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some() || signature.asyncness.is_some()
        || signature.unsafety.is_some() || signature.abi.is_some() || !valid_return
    {
        return syn::Error::new_spanned(signature, "schema requires fn() -> &'static str")
            .to_compile_error().into();
    }
    let name = &signature.ident;
    quote! {
        #function
        #[cfg(all(loom_core, not(feature = "loom-dependency")))]
        #[unsafe(export_name = "loom_schema")]
        pub extern "C" fn __loom_schema_export() -> u64 {
            ::loom::core::response(Ok(#name()))
        }
    }.into()
}

/// Export one free function as the component's callable definition.
#[proc_macro_attribute]
pub fn def(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut definition_hash = syn::LitStr::new("$self", proc_macro2::Span::call_site());
    let mut declared_effects: Option<Vec<syn::LitStr>> = None;
    // The host checker validates residual rows. Accept the same declaration here
    // without presenting it as a Rust type-system guarantee.
    if !attr.is_empty() {
        let arguments = parse_macro_input!(attr with syn::punctuated::Punctuated::<syn::MetaNameValue, syn::Token![,]>::parse_terminated);
        let mut seen = std::collections::BTreeSet::new();
        for argument in arguments {
            let key = argument.path.get_ident().map(ToString::to_string).unwrap_or_default();
            if !seen.insert(key.clone()) {
                return syn::Error::new_spanned(argument, "duplicate definition attribute").to_compile_error().into();
            }
            match (key.as_str(), &argument.value) {
                ("hash", syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(hash), .. })) => definition_hash = hash.clone(),
                ("effects", syn::Expr::Array(array)) if array.elems.iter().all(|value| matches!(value, syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(label), .. }) if !label.value().is_empty() && label.value() != "*")) => {
                    declared_effects = Some(array.elems.iter().filter_map(|value| {
                        if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(label), .. }) = value { Some(label.clone()) } else { None }
                    }).collect());
                },
                _ => return syn::Error::new_spanned(argument, "expected hash = \"definition hash\" or effects = [\"label\"]").to_compile_error().into(),
            }
        }
    }
    let declared_metadata = match declared_effects {
        Some(labels) => quote!(::loom::serde_json::json!([#(#labels),*])),
        None => quote!(::loom::serde_json::Value::Null),
    };
    let function = parse_macro_input!(item as ItemFn);
    if function.sig.asyncness.is_some() || !function.sig.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &function.sig,
            "loom definitions must be synchronous and non-generic",
        )
        .to_compile_error()
        .into();
    }
    let name = &function.sig.ident;
    let component = format_ident!("__LoomDefinition");
    let mut decode = Vec::new();
    let mut arguments = Vec::new();
    let mut argument_types = Vec::new();
    let mut parameter_signatures = Vec::new();
    for (index, input) in function.sig.inputs.iter().enumerate() {
        let FnArg::Typed(argument) = input else {
            return syn::Error::new_spanned(input, "free function required")
                .to_compile_error()
                .into();
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return syn::Error::new_spanned(argument, "use named arguments")
                .to_compile_error()
                .into();
        };
        let variable = &pattern.ident;
        let ty = &argument.ty;
        decode.push(quote! { let #variable: #ty = ::loom::serde_json::from_value(values.get(#index).cloned().ok_or_else(|| format!("missing argument {}", #index))?).map_err(|error| error.to_string())?; });
        arguments.push(variable);
        argument_types.push(ty);
        let parameter_name = variable.to_string();
        let shape = type_shape(ty);
        parameter_signatures
            .push(quote! { ::loom::serde_json::json!({"name": #parameter_name, "shape": #shape}) });
    }
    let count = arguments.len();
    let def_constant = format_ident!("{}_DEF", name.to_string().to_uppercase());
    let output = &function.sig.output;
    let signature = format_ident!("{}_signature", name);
    let export_name = name.to_string();
    let return_shape = match output {
        syn::ReturnType::Default => quote!(::loom::serde_json::json!({"type":"null"})),
        syn::ReturnType::Type(_, ty) => type_shape(ty),
    };
    let invocation = if count == 1 {
        quote! { pub const #def_constant: ::loom::Def<fn(#(#argument_types),*) #output> = ::loom::Def::new(#definition_hash); }
    } else {
        let pascal_name: String = name
            .to_string()
            .split('_')
            .map(|part| {
                let mut chars = part.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect();
        let args_name = format_ident!("{}Args", pascal_name);
        let invocation_name = format_ident!("{}Invocation", pascal_name);
        let return_type = match output {
            syn::ReturnType::Default => quote!(()),
            syn::ReturnType::Type(_, ty) => quote!(#ty),
        };
        quote! {
            pub struct #args_name { #(pub #arguments: #argument_types),* }
            pub struct #invocation_name;
            impl ::loom::Invocation for #invocation_name {
                type Args = #args_name;
                type Output = #return_type;
                fn arguments(args: #args_name) -> Result<Vec<::loom::Value>, ::loom::EffectError> {
                    let #args_name { #(#arguments),* } = args;
                    Ok(vec![#(::loom::serde_json::to_value(#arguments).map_err(|error| error.to_string())?),*])
                }
            }
            pub const #def_constant: ::loom::Def<#invocation_name> = ::loom::Def::new(#definition_hash);
        }
    };
    quote! {
        #function
        pub fn #signature() -> ::loom::Value {
            ::loom::serde_json::json!({"effects":{"labels":[],"unknown":true,"declared":#declared_metadata},"exports":[{"name":#export_name,"params":[#(#parameter_signatures),*],"returns":#return_shape,"effects":{"labels":[],"unknown":true,"declared":#declared_metadata}}]})
        }
        #invocation
        pub struct #component;
        #[cfg(not(feature = "loom-dependency"))]
        impl ::loom::bindings::Guest for #component {
            fn run(_state: Vec<u8>, _msg: Vec<u8>) -> Result<Vec<u8>, String> { Err("free definition has no actor handler".into()) }
            fn fold(_state: Vec<u8>, _event: Vec<u8>) -> Vec<u8> { panic!("free definition has no fold") }
            fn call(_def: Vec<u8>, args: Vec<u8>) -> Result<Vec<u8>, String> {
                let value: ::loom::Value = ::loom::decode_host(&args)?;
                let values = match value { ::loom::Value::Array(values) => values, value if #count == 1 => vec![value], ::loom::Value::Null if #count == 0 => vec![], _ => return Err("arguments must be an array".into()) };
                if values.len() != #count { return Err(format!("expected {} arguments, got {}", #count, values.len())); }
                #(#decode)*
                ::loom::encode(&#name(#(#arguments),*))
            }
        }
        #[cfg(not(feature = "loom-dependency"))]
        ::loom::bindings::export!(#component);
    }.into()
}

/// Export an Actor implementation. Place this attribute on its named struct.
#[proc_macro_attribute]
pub fn actor(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        let declaration = parse_macro_input!(attr as syn::MetaNameValue);
        let valid = declaration.path.is_ident("effects") && matches!(&declaration.value,
            syn::Expr::Array(array) if array.elems.iter().all(|value| matches!(value,
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(label), .. }) if !label.value().is_empty() && label.value() != "*")));
        if !valid {
            return syn::Error::new_spanned(declaration, "expected effects = [\"label\"]").to_compile_error().into();
        }
    }
    let structure = parse_macro_input!(item as ItemStruct);
    let name = &structure.ident;
    quote! {
        #structure
        #[cfg(not(feature = "loom-dependency"))]
        impl ::loom::bindings::Guest for #name {
            #[cfg(loom_core)]
            fn init() -> Result<Vec<u8>, String> { ::loom::encode(&<Self as ::loom::Actor>::init()) }
            fn run(state: Vec<u8>, msg: Vec<u8>) -> Result<Vec<u8>, String> {
                let state = if ::loom::decode_host::<::loom::Value>(&state)?.is_null() { <Self as ::loom::Actor>::init() } else { ::loom::decode_host(&state)? };
                let msg = ::loom::decode_host(&msg)?;
                ::loom::encode(&<Self as ::loom::Actor>::handle(&state, msg))
            }
            fn fold(state: Vec<u8>, event: Vec<u8>) -> Vec<u8> {
                let state = if ::loom::decode_host::<::loom::Value>(&state).expect("invalid state CBOR").is_null() { <Self as ::loom::Actor>::init() } else { ::loom::decode_host(&state).expect("invalid actor state") };
                let event = ::loom::decode_host(&event).expect("invalid actor event");
                ::loom::encode(&<Self as ::loom::Actor>::fold(state, &event)).expect("invalid folded state")
            }
            fn call(_def: Vec<u8>, _args: Vec<u8>) -> Result<Vec<u8>, String> { Err("actor definition has no free function".into()) }
        }
        #[cfg(not(feature = "loom-dependency"))]
        ::loom::bindings::export!(#name);
    }.into()
}

fn type_shape(ty: &syn::Type) -> proc_macro2::TokenStream {
    let primitive = match ty {
        syn::Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return quote!(::loom::serde_json::json!({"type":"value"}));
            };
            match segment.ident.to_string().as_str() {
                "bool" => "boolean",
                "String" | "str" => "string",
                "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize"
                | "f32" | "f64" => "number",
                "Vec" => {
                    if let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments
                        && let Some(syn::GenericArgument::Type(item)) = arguments.args.first()
                    {
                        let shape = type_shape(item);
                        return quote!(::loom::serde_json::json!({"type":"array","items":#shape}));
                    }
                    "value"
                }
                _ => "value",
            }
        }
        syn::Type::Reference(reference) => return type_shape(&reference.elem),
        syn::Type::Array(array) => {
            let shape = type_shape(&array.elem);
            return quote!(::loom::serde_json::json!({"type":"array","items":#shape}));
        }
        syn::Type::Slice(slice) => {
            let shape = type_shape(&slice.elem);
            return quote!(::loom::serde_json::json!({"type":"array","items":#shape}));
        }
        _ => "value",
    };
    quote!(::loom::serde_json::json!({"type":#primitive}))
}
