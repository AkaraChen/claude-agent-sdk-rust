use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Error, Expr, ExprLit, FnArg, Ident, ItemFn, Lit, LitInt, LitStr, MetaNameValue, Pat, PatType,
    Result, Token, Type, Visibility, parse_macro_input,
};

struct ToolArgs {
    name: LitStr,
    description: LitStr,
    tool_fn: Option<Ident>,
    annotations: Option<Expr>,
    max_result_size_chars: Option<LitInt>,
}

impl Parse for ToolArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let values = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        let mut name = None;
        let mut description = None;
        let mut tool_fn = None;
        let mut annotations = None;
        let mut max_result_size_chars = None;

        for value in values {
            let Some(ident) = value.path.get_ident().map(ToString::to_string) else {
                return Err(Error::new_spanned(value.path, "expected identifier key"));
            };
            match ident.as_str() {
                "name" => name = Some(expect_lit_str(value.value, "name")?),
                "description" => {
                    description = Some(expect_lit_str(value.value, "description")?);
                }
                "tool_fn" => {
                    let fn_name = expect_lit_str(value.value, "tool_fn")?;
                    tool_fn = Some(Ident::new(&fn_name.value(), fn_name.span()));
                }
                "annotations" => annotations = Some(value.value),
                "max_result_size_chars" => {
                    max_result_size_chars =
                        Some(expect_lit_int(value.value, "max_result_size_chars")?);
                }
                other => {
                    return Err(Error::new_spanned(
                        value.path,
                        format!("unsupported sdk_tool argument `{other}`"),
                    ));
                }
            }
        }

        Ok(Self {
            name: name.ok_or_else(|| Error::new(input.span(), "missing `name = \"...\"`"))?,
            description: description
                .ok_or_else(|| Error::new(input.span(), "missing `description = \"...\"`"))?,
            tool_fn,
            annotations,
            max_result_size_chars,
        })
    }
}

fn expect_lit_str(expr: Expr, key: &str) -> Result<LitStr> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Ok(value),
        other => Err(Error::new_spanned(
            other,
            format!("`{key}` must be a string literal"),
        )),
    }
}

fn expect_lit_int(expr: Expr, key: &str) -> Result<LitInt> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Int(value),
            ..
        }) => Ok(value),
        other => Err(Error::new_spanned(
            other,
            format!("`{key}` must be an integer literal"),
        )),
    }
}

#[proc_macro_attribute]
pub fn sdk_tool(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as ToolArgs);
    let input_fn = parse_macro_input!(item as ItemFn);

    match expand_sdk_tool(args, input_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_sdk_tool(args: ToolArgs, input_fn: ItemFn) -> Result<proc_macro2::TokenStream> {
    let fn_name = input_fn.sig.ident.clone();
    let vis = input_fn.vis.clone();
    let tool_fn = args
        .tool_fn
        .unwrap_or_else(|| format_ident!("{}_tool", fn_name));
    let arg_ty = single_typed_arg(&input_fn)?;
    let name = args.name;
    let description = args.description;
    let annotations = match args.annotations {
        Some(expr) => quote! { Some(#expr) },
        None => quote! { None },
    };
    let max_result = args
        .max_result_size_chars
        .map(|value| quote! { .with_max_result_size_chars(#value) })
        .unwrap_or_default();

    let factory_vis = factory_visibility(&vis);
    Ok(quote! {
        #input_fn

        #factory_vis fn #tool_fn() -> ::claude_agent_sdk::SdkMcpTool {
            ::claude_agent_sdk::tool::<#arg_ty, _, _, _>(
                #name,
                #description,
                #fn_name,
                #annotations,
            )#max_result
        }
    })
}

fn single_typed_arg(input_fn: &ItemFn) -> Result<Type> {
    if input_fn.sig.inputs.len() != 1 {
        return Err(Error::new_spanned(
            &input_fn.sig.inputs,
            "sdk_tool functions must take exactly one typed argument",
        ));
    }
    match input_fn.sig.inputs.first().unwrap() {
        FnArg::Typed(PatType { pat, ty, .. }) => {
            if matches!(pat.as_ref(), Pat::Ident(_)) {
                Ok((**ty).clone())
            } else {
                Err(Error::new_spanned(
                    pat,
                    "sdk_tool argument must be an identifier pattern",
                ))
            }
        }
        FnArg::Receiver(receiver) => Err(Error::new_spanned(
            receiver,
            "sdk_tool functions cannot take self",
        )),
    }
}

fn factory_visibility(vis: &Visibility) -> proc_macro2::TokenStream {
    match vis {
        Visibility::Public(public) => quote! { #public },
        Visibility::Restricted(restricted) => quote! { #restricted },
        Visibility::Inherited => quote! {},
    }
}
