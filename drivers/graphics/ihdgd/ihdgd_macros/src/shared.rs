use syn::Token;
use syn::parse::{Parse, ParseStream};

pub(crate) mod kw {
    syn::custom_keyword!(field);
    syn::custom_keyword!(flag);
    syn::custom_keyword!(reg);
}

#[derive(Clone, Copy)]
pub(crate) enum FieldKind {
    Let(Token![let]),
    Reg(kw::reg),
}

impl Parse for FieldKind {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let lookahead = input.lookahead1();
        if lookahead.peek(Token![let]) {
            input.parse().map(Self::Let)
        } else if lookahead.peek(kw::reg) {
            input.parse().map(Self::Reg)
        } else {
            Err(lookahead.error())
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RegFieldKind {
    Field(kw::field),
    Flag(kw::flag),
    Enum(Token![enum]),
}

impl Parse for RegFieldKind {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let lookahead = input.lookahead1();
        if lookahead.peek(kw::field) {
            input.parse().map(Self::Field)
        } else if lookahead.peek(kw::flag) {
            input.parse().map(Self::Flag)
        } else if lookahead.peek(Token![enum]) {
            input.parse().map(Self::Enum)
        } else {
            Err(lookahead.error())
        }
    }
}
