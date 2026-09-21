use syn::parse::{Parse, ParseStream};
use syn::{LitInt, Token};

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

pub(crate) struct ShiftedMask {
    pub(crate) _eq: Token![=],
    pub(crate) mask: LitInt,
    pub(crate) _shl: Token![<<],
    pub(crate) shift: LitInt,
}

impl Parse for ShiftedMask {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let this = ShiftedMask {
            _eq: input.parse::<Token![=]>()?,
            mask: input.parse::<LitInt>()?,
            _shl: input.parse::<Token![<<]>()?,
            shift: input.parse::<LitInt>()?,
        };

        // Check the mask is of form 0b1111 without zeros anywhere before the first one
        let mask = this.mask.base10_parse::<u128>()?;
        assert_eq!(mask.highest_one().unwrap() + 1, mask.count_ones());

        Ok(this)
    }
}

impl ShiftedMask {
    pub(crate) fn parse_flag(input: ParseStream) -> syn::Result<Self> {
        let this = Self::parse(input)?;
        assert_eq!(this.mask.base10_parse::<u128>().unwrap(), 1);
        Ok(this)
    }
}
