use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Expr, FnArg, ItemFn, Meta, ReturnType, parse_macro_input, punctuated::Punctuated, token::Comma,
};

#[proc_macro_attribute]
pub fn ps4_syscall(args: TokenStream, input: TokenStream) -> TokenStream {
    let nested_metas = parse_macro_input!(args with Punctuated::<Meta, Comma>::parse_terminated);
    let input_fn = parse_macro_input!(input as ItemFn);

    let mut id_expr: Option<Expr> = None;
    let mut lib_expr: Option<Expr> = None;
    let mut manual_names: Vec<Expr> = Vec::new();

    for meta in nested_metas {
        if let Meta::NameValue(nv) = meta {
            if nv.path.is_ident("id") {
                id_expr = Some(nv.value);
            } else if nv.path.is_ident("lib") {
                lib_expr = Some(nv.value);
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

    let return_conversion = match output {
        ReturnType::Default => quote! { 0u64 },
        ReturnType::Type(_, _) => quote! { result as u64 },
    };

    let wrapper_ident = format_ident!("__ps4_wrapper_{}", fn_name_ident);
    let names_static_ident = format_ident!("__PS4_NAMES_{}", fn_name_ident);

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

        inventory::submit! {
            crate::registry::HleSyscallDef {
                id: #id,
                lib_name: #lib,
                names: &#names_static_ident,
                handler: #wrapper_ident,
            }
        }
    };

    TokenStream::from(expanded)
}
