use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::token::Bracket;
use syn::{Ident, LitInt, Token, Type, Visibility, braced, bracketed, parse_macro_input, token};

use crate::shared::{FieldKind, RegFieldKind, kw};

struct Input {
    vis: Visibility,
    _struct_token: Token![struct],
    name: Ident,
    _brace_token: token::Brace,
    fields: Punctuated<InputField, Token![,]>,
}

impl Parse for Input {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        Ok(Input {
            vis: input.parse()?,
            _struct_token: input.parse()?,
            name: input.parse()?,
            _brace_token: braced!(content in input),
            fields: content.parse_terminated(InputField::parse, Token![,])?,
        })
    }
}

struct InputField {
    vis: Visibility,
    name: Ident,
    count: Option<(Bracket, LitInt)>,
    optional: Option<Token![?]>,
    _colon: Token![:],
    type_: Type,
    kind: InputFieldKind,
}

enum InputFieldKind {
    Let {
        _let: Token![let],
    },
    Reg {
        _reg: kw::reg,
        fields: Option<(token::Brace, Punctuated<InputFieldRegField, Token![,]>)>,
    },
}

impl Parse for InputField {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let vis = input.parse()?;
        let kind: FieldKind = input.parse()?;
        Ok(InputField {
            vis,
            name: input.parse()?,
            count: input.call(|input| {
                if input.peek(token::Bracket) {
                    let count;
                    Ok(Some((bracketed!(count in input), count.parse::<LitInt>()?)))
                } else {
                    Ok(None)
                }
            })?,
            optional: input.parse()?,
            _colon: input.parse()?,
            type_: input.parse()?,
            kind: match kind {
                FieldKind::Let(_let) => InputFieldKind::Let { _let },
                FieldKind::Reg(_reg) => InputFieldKind::Reg {
                    _reg,
                    fields: input.call(|input| {
                        if input.peek(token::Brace) {
                            let fields;
                            Ok(Some((
                                braced!(fields in input),
                                fields.parse_terminated(InputFieldRegField::parse, Token![,])?,
                            )))
                        } else {
                            Ok(None)
                        }
                    })?,
                },
            },
        })
    }
}

struct InputFieldRegField {
    name: Ident,
    kind: InputFieldRegFieldKind,
}

enum InputFieldRegFieldKind {
    Field {
        _field: kw::field,
    },
    Flag {
        _flag: kw::flag,
    },
    Enum {
        _enum: Token![enum],
        _brace_token: token::Brace,
        variants: Punctuated<Ident, Token![,]>,
    },
}

impl Parse for InputFieldRegField {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kind: RegFieldKind = input.parse()?;
        let name = input.parse()?;
        let kind = match kind {
            RegFieldKind::Field(_field) => InputFieldRegFieldKind::Field { _field },
            RegFieldKind::Flag(_flag) => InputFieldRegFieldKind::Flag { _flag },
            RegFieldKind::Enum(_enum) => {
                let variants;
                InputFieldRegFieldKind::Enum {
                    _enum,
                    _brace_token: braced!(variants in input),
                    variants: variants.parse_terminated(Ident::parse, Token![,])?,
                }
            }
        };
        Ok(Self { name, kind })
    }
}

impl InputFieldRegField {
    fn generate(self, vis: &Visibility, reg_type: &Type) -> Vec<proc_macro2::TokenStream> {
        let reg_field_name = self.name;
        match self.kind {
            InputFieldRegFieldKind::Field { _field } => {
                let mask = Ident::new(&format!("{reg_field_name}_mask"), reg_field_name.span());
                let shift = Ident::new(&format!("{reg_field_name}_shift"), reg_field_name.span());
                vec![
                    quote! { #vis #mask: #reg_type },
                    quote! { #vis #shift: #reg_type },
                ]
            }
            InputFieldRegFieldKind::Flag { _flag } => {
                vec![quote! { #vis #reg_field_name: #reg_type }]
            }
            InputFieldRegFieldKind::Enum {
                _enum,
                _brace_token,
                variants,
            } => {
                let mask = Ident::new(&format!("{reg_field_name}_mask"), reg_field_name.span());
                let mut fields = vec![quote! { #vis #mask: #reg_type }];
                for variant in variants {
                    let variant =
                        Ident::new(&format!("{reg_field_name}_{variant}"), variant.span());
                    fields.push(quote! { #vis #variant: #reg_type })
                }
                fields
            }
        }
    }
}

pub(crate) fn define_regs(tokens: TokenStream) -> TokenStream {
    let input = parse_macro_input!(tokens as Input);
    let struct_vis = input.vis;
    let struct_name = input.name;

    let mut fields = vec![];
    let mut reg_types = vec![];
    for field in input.fields {
        let field_vis = field.vis;
        let field_name = field.name;
        let field_type = field.type_;
        let mut type_ = match field.kind {
            InputFieldKind::Let { _let: _ } => {
                quote! { #field_type }
            }
            InputFieldKind::Reg { _reg: _, fields } => {
                if let Some((_, reg_fields)) = fields {
                    let reg_type_name =
                        Ident::new(&format!("{struct_name}_{field_name}"), field_name.span());
                    let reg_fields = reg_fields
                        .into_iter()
                        .flat_map(|reg_field| reg_field.generate(&field_vis, &field_type));
                    reg_types.push(quote! {
                        #[allow(non_camel_case_types)]
                        #field_vis struct #reg_type_name {
                            #field_vis reg: MmioPtr<#field_type>,
                            #(#reg_fields),*
                        }
                    });
                    quote! { #reg_type_name }
                } else {
                    quote! { MmioPtr<#field_type> }
                }
            }
        };
        if let Some((_, count)) = field.count {
            type_ = quote! { [#type_; #count] }
        };
        if field.optional.is_some() {
            type_ = quote! { Option<#type_> };
        }
        fields.push(quote! { #field_vis #field_name: #type_ });
    }

    quote! {
        #struct_vis struct #struct_name {
            #(#fields),*
        }

        #(#reg_types)*
    }
    .into()
}
