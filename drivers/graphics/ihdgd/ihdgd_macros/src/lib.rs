use proc_macro::TokenStream;

mod define_regs;
mod shared;

#[proc_macro]
pub fn define_regs(tokens: TokenStream) -> TokenStream {
    define_regs::define_regs(tokens)
}
