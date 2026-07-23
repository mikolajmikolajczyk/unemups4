use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Expr, FnArg, ItemFn, Meta, ReturnType, parse_macro_input, punctuated::Punctuated, token::Comma,
};

/// True if `ty` is a path type whose final segment is `Result` (task-191). Used to
/// decide whether the generated wrapper must project an `Err(Errno)` into an ABI.
fn type_is_result(ty: &syn::Type) -> bool {
    if let syn::Type::Path(tp) = ty
        && let Some(seg) = tp.path.segments.last()
    {
        return seg.ident == "Result";
    }
    false
}

#[proc_macro_attribute]
pub fn ps4_syscall(args: TokenStream, input: TokenStream) -> TokenStream {
    let nested_metas = parse_macro_input!(args with Punctuated::<Meta, Comma>::parse_terminated);
    let input_fn = parse_macro_input!(input as ItemFn);

    let mut id_expr: Option<Expr> = None;
    let mut lib_expr: Option<Expr> = None;
    let mut manual_names: Vec<Expr> = Vec::new();
    // Raw NIDs this handler also answers to, for imports whose NAME we do not have. A NID is
    // a one-way hash, so a symbol missing from the name table can only ever be bound by its
    // hash — see the `nids` arm below.
    let mut manual_nids: Vec<Expr> = Vec::new();
    // task-191: which error ABI the failing handler serves. `sce` (default) encodes a
    // Result's Err(Errno) as the positive `0x8002_00xx` SCE code; `posix` encodes it as
    // the negated errno (the OpenOrbis libc-wrapper error path). Only consulted when the
    // handler returns a `Result` — non-Result handlers are unaffected.
    let mut abi_is_posix = false;

    for meta in nested_metas {
        if let Meta::NameValue(nv) = meta {
            if nv.path.is_ident("id") {
                id_expr = Some(nv.value);
            } else if nv.path.is_ident("lib") {
                lib_expr = Some(nv.value);
            } else if nv.path.is_ident("abi") {
                // value is a bare ident: `sce` or `posix`.
                let abi_ident = match &nv.value {
                    Expr::Path(p) => p.path.get_ident().map(|i| i.to_string()),
                    _ => None,
                };
                match abi_ident.as_deref() {
                    Some("sce") => abi_is_posix = false,
                    Some("posix") => abi_is_posix = true,
                    _ => panic!("'abi' must be `sce` or `posix`"),
                }
            } else if nv.path.is_ident("name") {
                manual_names.push(nv.value);
            } else if nv.path.is_ident("names") {
                if let Expr::Array(array_expr) = nv.value {
                    for elem in array_expr.elems {
                        manual_names.push(elem);
                    }
                } else {
                    panic!("'names' must be an array");
                }
            } else if nv.path.is_ident("nids") {
                if let Expr::Array(array_expr) = nv.value {
                    for elem in array_expr.elems {
                        manual_nids.push(elem);
                    }
                } else {
                    panic!("'nids' must be an array");
                }
            }
        }
    }

    let id = id_expr.expect("Missing 'id'");
    let lib = lib_expr.expect("Missing 'lib'");
    let fn_name_ident = &input_fn.sig.ident;
    let fn_name_str = fn_name_ident.to_string();

    // names list: manual names first, then the rust fn name
    let mut all_names_exprs = Vec::new();

    for name_expr in manual_names {
        all_names_exprs.push(quote! { #name_expr });
    }

    let fn_lit = syn::LitStr::new(&fn_name_str, fn_name_ident.span());
    all_names_exprs.push(quote! { #fn_lit });

    let array_len = all_names_exprs.len();
    let nids_len = manual_nids.len();

    // wrapper that pulls args out of the context and calls the fn
    let inputs = &input_fn.sig.inputs;
    let output = &input_fn.sig.output;
    let mut wrapper_body = Vec::new();
    let mut call_args = Vec::new();

    for (i, arg) in inputs.iter().enumerate() {
        if let FnArg::Typed(pat_type) = arg {
            let ty = &pat_type.ty;
            let arg_name = format_ident!("arg_{}", i);
            let ctx_method = format_ident!("arg{}", i);
            wrapper_body.push(quote! {
                let #arg_name: #ty = ctx.#ctx_method();
            });
            call_args.push(quote! { #arg_name });
        }
    }

    // task-191: a handler whose return type is `Result<_, Errno>` gets its Err arm
    // projected into the requested ABI at macro-expansion time (the branch is chosen
    // statically — no runtime `if`). Every non-Result handler keeps the byte-for-byte
    // legacy conversion so the existing handlers are unaffected.
    let returns_result = match output {
        ReturnType::Type(_, ty) => type_is_result(ty),
        ReturnType::Default => false,
    };

    let return_conversion = match output {
        ReturnType::Default => quote! { 0u64 },
        ReturnType::Type(_, _) if returns_result => {
            // The Err arm differs by ABI. `posix` also writes the guest errno TLS slot
            // (`ps4_cpu::set_errno`) BEFORE returning the negated errno: retail Sony/Mono
            // wrappers IGNORE the return value and read `*__error()` instead, so a bare
            // `-2` return left the slot stale and Mono's "unknown error" formatter fed
            // NULL to `%s` -> `strlen(NULL)` crash (task-191). OpenOrbis libc reads the
            // `-errno` RETURN and negates it into errno itself, so the extra TLS write is
            // harmless/consistent for it. `sce` returns the positive `0x8002_00xx` code
            // and does NOT touch errno. Both projected statically at expansion.
            let err_arm = if abi_is_posix {
                quote! {
                    Err(e) => {
                        ps4_cpu::set_errno(e.0);
                        ps4_core::errno::Errno::to_posix(e) as u64
                    }
                }
            } else {
                quote! {
                    Err(e) => ps4_core::errno::Errno::to_sce(e) as u64,
                }
            };
            quote! {
                match result {
                    Ok(v) => v as u64,
                    #err_arm
                }
            }
        }
        ReturnType::Type(_, _) if abi_is_posix => {
            // task-191: a non-`Result` handler annotated `abi = posix` (e.g. `posix_pread`/
            // `posix_pwrite`, which return `i64`) already carries the negated errno in its
            // return value on failure. Sony/Mono libc wrappers IGNORE that return and read
            // `*__error()` instead, so mirror the negative return into the guest errno TLS
            // slot exactly as the `Result` posix arm does — otherwise the slot goes stale and
            // Mono's "unknown error" formatter feeds NULL to `%s` (`strlen(NULL)` crash). A
            // non-negative return is a success and leaves errno untouched, so the u64 value
            // returned is byte-identical to the legacy `result as u64` arm.
            quote! {
                let r = result as i64;
                if r < 0 {
                    ps4_cpu::set_errno((-r) as i32);
                }
                r as u64
            }
        }
        ReturnType::Type(_, _) => quote! { result as u64 },
    };

    let wrapper_ident = format_ident!("__ps4_wrapper_{}", fn_name_ident);
    let names_static_ident = format_ident!("__PS4_NAMES_{}", fn_name_ident);
    let nids_static_ident = format_ident!("__PS4_NIDS_{}", fn_name_ident);

    let expanded = quote! {
        #input_fn

        #[allow(non_snake_case)]
        fn #wrapper_ident(ctx: &mut NativeContext) -> u64 {
            #(#wrapper_body)*
            let result = #fn_name_ident(#(#call_args),*);
            #return_conversion
        }

        #[allow(non_upper_case_globals)]
        static #names_static_ident: [&'static str; #array_len] = [
            #(
                #all_names_exprs as &'static str
            ),*
        ];

        #[allow(non_upper_case_globals)]
        static #nids_static_ident: [&'static str; #nids_len] = [
            #(
                #manual_nids as &'static str
            ),*
        ];

        inventory::submit! {
            crate::registry::HleSyscallDef {
                id: #id,
                lib_name: #lib,
                names: &#names_static_ident,
                nids: &#nids_static_ident,
                handler: #wrapper_ident,
            }
        }
    };

    TokenStream::from(expanded)
}
