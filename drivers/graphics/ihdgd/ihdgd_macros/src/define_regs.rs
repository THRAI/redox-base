use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::token::Bracket;
use syn::{Ident, LitInt, Token, Type, Visibility, braced, bracketed, parse_macro_input, token};

use crate::shared::{FieldKind, RegFieldKind, ShiftedMask, kw};

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
        default: Option<ShiftedMask>,
    },
    Flag {
        _flag: kw::flag,
        default: Option<ShiftedMask>,
    },
    Enum {
        _enum: Token![enum],
        default: Option<ShiftedMask>,
        _brace_token: token::Brace,
        variants: Punctuated<(Ident, Option<(Token![=], LitInt)>), Token![,]>,
    },
}

impl Parse for InputFieldRegField {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kind: RegFieldKind = input.parse()?;
        let name = input.parse()?;
        let kind = match kind {
            RegFieldKind::Field(_field) => {
                let default = if input.peek(Token![=]) {
                    Some(ShiftedMask::parse(input)?)
                } else {
                    None
                };
                InputFieldRegFieldKind::Field { _field, default }
            }
            RegFieldKind::Flag(_flag) => {
                let default = if input.peek(Token![=]) {
                    Some(ShiftedMask::parse_flag(input)?)
                } else {
                    None
                };
                InputFieldRegFieldKind::Flag { _flag, default }
            }
            RegFieldKind::Enum(_enum) => {
                let default = if input.peek(Token![=]) {
                    Some(ShiftedMask::parse(input)?)
                } else {
                    None
                };
                let variants;
                InputFieldRegFieldKind::Enum {
                    _enum,
                    default,
                    _brace_token: braced!(variants in input),
                    variants: variants.parse_terminated(
                        |input| {
                            let variant = Ident::parse(input)?;
                            let default = if input.peek(Token![=]) {
                                Some((input.parse::<Token![=]>()?, input.parse::<LitInt>()?))
                            } else {
                                None
                            };
                            Ok((variant, default))
                        },
                        Token![,],
                    )?,
                }
            }
        };
        Ok(Self { name, kind })
    }
}

impl InputFieldRegField {
    fn generate(
        self,
        vis: &Visibility,
        field_type: &Type,
    ) -> (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>) {
        let reg_field_name = self.name;
        match self.kind {
            InputFieldRegFieldKind::Field { _field, default } => {
                let mask = Ident::new(&format!("{reg_field_name}_mask"), reg_field_name.span());
                let shift = Ident::new(&format!("{reg_field_name}_shift"), reg_field_name.span());
                let set = Ident::new(&format!("set_{reg_field_name}"), reg_field_name.span());
                if let Some(ShiftedMask {
                    _eq,
                    mask,
                    _shl,
                    shift,
                }) = default
                {
                    (
                        vec![],
                        vec![
                            quote! { #vis fn #reg_field_name(&self) -> #field_type {
                                (self.0 >> #shift) & #mask
                            }},
                            quote! { #vis fn #set(mut self, data: #field_type) -> Self {
                                self.0 &= !(#mask << #shift);
                                self.0 |= data << #shift;
                                self
                            }},
                        ],
                    )
                } else {
                    (
                        vec![
                            quote! { #vis #mask: #field_type },
                            quote! { #vis #shift: #field_type },
                        ],
                        vec![
                            quote! { #vis fn #reg_field_name(&self) -> #field_type {
                                (self.0 & self.1.#mask) >> self.1.#shift
                            }},
                            quote! { #vis fn #set(mut self, data: #field_type) -> Self {
                                self.0 &= !self.1.#mask;
                                self.0 |= data << self.1.#shift;
                                self
                            }},
                        ],
                    )
                }
            }
            InputFieldRegFieldKind::Flag { _flag, default } => {
                let set = Ident::new(&format!("set_{reg_field_name}"), reg_field_name.span());
                if let Some(ShiftedMask {
                    _eq,
                    mask: _,
                    _shl,
                    shift,
                }) = default
                {
                    (
                        vec![],
                        vec![
                            quote! { #vis fn #reg_field_name(&self) -> bool {
                                self.0 & (1 << #shift) != 0
                            }},
                            quote! { #vis fn #set(mut self, val: bool) -> Self {
                                self.0 &= !(1 << #shift);
                                if val {
                                    self.0 |= (1 << #shift);
                                }
                                self
                            }},
                        ],
                    )
                } else {
                    (
                        vec![quote! { #vis #reg_field_name: #field_type }],
                        vec![
                            quote! { #vis fn #reg_field_name(&self) -> bool {
                                self.0 & self.1.#reg_field_name != 0
                            }},
                            quote! { #vis fn #set(mut self, val: bool) -> Self {
                                self.0 &= !self.1.#reg_field_name;
                                if val {
                                    self.0 |= self.1.#reg_field_name;
                                }
                                self
                            }},
                        ],
                    )
                }
            }
            InputFieldRegFieldKind::Enum {
                _enum,
                default,
                _brace_token,
                variants,
            } => {
                let mut fields = vec![];
                let mut methods = vec![];
                let (mask, shift) = if let Some(default) = default {
                    let mask = default.mask;
                    let shift = default.shift;
                    (quote! { (#mask << #shift) }, quote! { #shift })
                } else {
                    let mask = Ident::new(&format!("{reg_field_name}_mask"), reg_field_name.span());
                    let shift =
                        Ident::new(&format!("{reg_field_name}_shift"), reg_field_name.span());
                    fields.push(quote! { #vis #mask: #field_type });
                    fields.push(quote! { #vis #shift: #field_type });
                    (quote! { self.1.#mask }, quote! { self.1.#shift })
                };

                for (variant, variant_default) in variants {
                    let variant =
                        Ident::new(&format!("{reg_field_name}_{variant}"), variant.span());
                    let set_variant = Ident::new(&format!("set_{variant}"), variant.span());
                    if let Some((_eq, variant_default)) = variant_default {
                        methods.push(quote! {
                            #vis fn #set_variant(mut self) -> Self {
                                self.0 &= !#mask;
                                self.0 |= #variant_default << #shift;
                                self
                            }
                        });
                    } else {
                        fields.push(quote! { #vis #variant: #field_type });
                        methods.push(quote! {
                        #vis fn #set_variant(mut self) -> Self {
                                self.0 &= !#mask;
                                self.0 |= self.1.#variant << #shift;
                            self
                        }
                        });
                    };
                }
                (fields, methods)
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
                    let reg_data_type_name = Ident::new(
                        &format!("{struct_name}_{field_name}Data"),
                        field_name.span(),
                    );
                    let (reg_fields, reg_data_methods): (Vec<_>, Vec<_>) = reg_fields
                        .into_iter()
                        .map(|reg_field| reg_field.generate(&field_vis, &field_type))
                        .unzip();
                    let reg_fields = reg_fields.into_iter().flatten();
                    let reg_data_methods = reg_data_methods.into_iter().flatten();
                    reg_types.push(quote! {
                        #[allow(non_camel_case_types)]
                        #field_vis struct #reg_type_name {
                            #field_vis reg: MmioPtr<#field_type>,
                            #(#reg_fields),*
                        }

                        impl #reg_type_name {
                            #field_vis fn read(&self) -> #reg_data_type_name {
                                #reg_data_type_name(self.reg.read(), self)
                            }

                            /// Write a new value to the register, discarding
                            /// the old value.
                            #field_vis fn write(&mut self, f: impl FnOnce(#reg_data_type_name) -> #reg_data_type_name) {
                                self.reg.write(f(#reg_data_type_name(0, self)).0);
                            }

                            /// Modify the contents of the register, preserving
                            /// all fields not explicitly changed.
                            #field_vis fn modify(&mut self, f: impl FnOnce(#reg_data_type_name) -> #reg_data_type_name) {
                                self.reg.write(f(self.read()).0);
                            }
                        }

                        #[derive(Copy, Clone)]
                        #[allow(non_camel_case_types)]
                        #field_vis struct #reg_data_type_name<'a>(#field_type, &'a #reg_type_name);

                        impl #reg_data_type_name<'_> {
                            #field_vis fn raw(&self) -> #field_type {
                                self.0
                            }

                            // FIXME remove these eventually
                            #field_vis fn or_raw(self, data: #field_type) -> Self {
                                Self(self.0 | data, self.1)
                            }

                            #field_vis fn and_raw(self, data: #field_type) -> Self {
                                Self(self.0 & data, self.1)
                            }

                            #(#reg_data_methods)*
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
