use proc_macro::TokenStream;

mod define_regs;
mod shared;

/// A macro that allows defining a set of registers together with subfields.
/// This will produce a set of structs you can fill with the correct locations
/// of registers and subfields, after which you can use the `.read()`,
/// `.write()` and `.modify()` methods on register fields.
///
/// # Example
///
/// ```rust
/// define_regs! {
///     pub struct MyRegs {
///         // Just a regular field
///         pub let name: &'static str,
///         // A register without subfields
///         pub reg some_reg: u32,
///         // A register with subfields
///         pub reg foo_ctl?: u32 {
///             // A single bit flag
///             flag bar,
///             // A multi bit subfield
///             field baz,
///             // A subfield with a fixed set of values it can take
///             enum quux {
///                 red,
///                 green,
///                 blue,
///             }
///         },
///         // An array of 8 identical registers
///         pub reg register_set[8]: u32,
///     }
/// }
/// ```
///
/// would produce something like
///
/// ```rust
/// pub struct MyRegs {
///     pub name: &'static str,
///     pub some_reg: MmioPtr<u32>,
///     pub foo_ctl: Option<MyRegs_foo_ctl>,
///     pub register_set: [MmioPtr<u32>; 8],
/// }
///
/// pub struct MyRegs_foo_ctl {
///     pub reg: MmioPtr<u32>,
///
///     pub bar: u32, // For example 1 << 10
///
///     pub baz_mask: u32, // For example 0b111 << 11
///     pub baz_shift: u32, // For example 11
///
///     pub quux_mask: u32, // For example 0b11 << 15
///     pub quux_red: u32, // For example 0b00 << 15
///     pub quux_green: u32, // For example 0b01 << 15
///     pub quux_blue: u32, // For example 0b10 << 15
/// }
///
/// impl MyRegs_foo_ctl {
///     pub fn read(&self) -> MyRegs_foo_ctlData { ... }
///     pub fn write(&mut self, f: impl FnOnce(MyRegs_foo_ctlData) -> MyRegs_foo_ctlData) { ... }
///     pub fn modify(&mut self, f: impl FnOnce(MyRegs_foo_ctlData) -> MyRegs_foo_ctlData) { ... }
/// }
///
/// #[derive(Copy, Clone)]
/// pub struct MyRegs_foo_ctlData<'a>(u32, &'a MyRegs_foo_ctl);
///
/// impl MyRegs_foo_ctlData<'_> {
///     pub fn raw(&self) -> u32 { ... }
///     pub fn or_raw(self, data: u32) -> Self { ... }
///     pub fn and_raw(self, data: u32) -> Self { ... }
///     pub fn bar(&self) -> bool { ... }
///     pub fn set_bar(mut self, val: bool) -> Self { ... }
///     pub fn baz(&self) -> u32 { ... }
///     pub fn set_baz(mut self, data: u32) -> Self { ... }
///     pub fn set_quux_red(mut self) -> Self { ... }
///     pub fn set_quux_green(mut self) -> Self { ... }
///     pub fn set_quux_blue(mut self) -> Self { ... }
/// }
/// ```
#[proc_macro]
pub fn define_regs(tokens: TokenStream) -> TokenStream {
    define_regs::define_regs(tokens)
}
