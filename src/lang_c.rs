//! Functions for converting Rust types to C types.

use Error;
use Level;
use common::{Lang, Outputs, append_output, check_no_mangle, check_repr_c, parse_attr,
             retrieve_docstring};
use petgraph::{Graph, algo};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::collections::btree_map::Entry;
use std::fmt::{self, Display, Formatter};
use std::path;
use syntax::{ast, codemap, print};
use syntax::abi::Abi;
use syntax::print::pprust;

pub struct LangC {
    lib_name: String,
    decls: BTreeMap<String, String>,
    deps: BTreeMap<String, Vec<String>>,
}

/// Compile the header declarations then add the needed `#include`s.
///
/// Currently includes:
///
/// - `stdint.h`
/// - `stdbool.h`
impl LangC {
    pub fn new() -> Self {
        Self {
            lib_name: "backend".to_owned(),
            decls: BTreeMap::new(),
            deps: BTreeMap::new(),
        }
    }

    /// Set the name of the native library.
    pub fn set_lib_name<T: Into<String>>(&mut self, name: T) {
        self.lib_name = name.into();
    }

    fn add_dependencies(&mut self, module: &[String], cty: &CType) -> Result<(), Error> {
        let deps = cty.dependencies();

        if !deps.is_empty() {
            let header = header_name(module, &self.lib_name)?;

            match self.deps.entry(header) {
                Entry::Occupied(o) => o.into_mut().extend(deps.into_iter()),
                Entry::Vacant(v) => {
                    let _ = v.insert(deps);
                }
            }
        }

        Ok(())
    }

    fn append_to_header(
        &mut self,
        buffer: String,
        module: &[String],
        outputs: &mut Outputs,
    ) -> Result<(), Error> {
        let header = header_name(module, &self.lib_name)?;
        append_output(buffer, &header, outputs);
        Ok(())
    }

    /// Transform a Rust FFI function into a C function decl
    pub fn transform_native_fn(
        &mut self,
        fn_decl: &ast::FnDecl,
        docs: &str,
        name: &str,
        module: &[String],
        outputs: &mut Outputs,
    ) -> Result<(), Error> {
        // Handle the case when the return type is a function pointer (which requires that the
        // entire declaration is wrapped by the function pointer type) by first creating the name
        // and parameters, then passing that whole thing to `rust_to_c`.
        let fn_args = fn_decl.inputs.clone();
        let mut args = Vec::new();

        // Arguments
        for arg in &fn_args {
            let arg_name = pprust::pat_to_string(&*arg.pat);
            let c_ty = rust_to_c(&arg.ty, &arg_name)?;
            self.add_dependencies(module, &c_ty.1)?;
            args.push(c_ty);
        }

        let buf = format!(
            "{}({})",
            name,
            if args.is_empty() {
                String::from("void")
            } else {
                args.into_iter()
                    .map(|cty| format!("{}", cty))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );

        // Generate return type
        let output_type = &fn_decl.output;
        let full_declaration = match *output_type {
            ast::FunctionRetTy::Ty(ref ty) if ty.node == ast::TyKind::Never => {
                return Err(Error {
                    level: Level::Error,
                    span: Some(ty.span),
                    message: "panics across a C boundary are naughty!".into(),
                });
            }
            ast::FunctionRetTy::Default(..) => format!("void {}", buf),
            ast::FunctionRetTy::Ty(ref ty) => {
                let c_ty = rust_to_c(&*ty, &buf)?;
                self.add_dependencies(module, &c_ty.1)?;
                format!("{}", c_ty)
            }
        };

        let mut output = String::new();
        output.push_str(docs);
        output.push_str(&full_declaration);
        output.push_str(";\n\n");

        append_output(output, &header_name(module, &self.lib_name)?, outputs);

        Ok(())
    }
}

impl Default for LangC {
    fn default() -> Self {
        Self::new()
    }
}

impl Lang for LangC {
    /// Convert `pub type A = B;` into `typedef B A;`.
    ///
    /// Aborts if A is generic.
    fn parse_ty(
        &mut self,
        item: &ast::Item,
        module: &[String],
        outputs: &mut Outputs,
    ) -> Result<(), Error> {
        let (_, docs) = parse_attr(&item.attrs, |_| true, |attr| retrieve_docstring(attr, ""));

        let mut buffer = String::new();
        buffer.push_str(&docs);

        let name = item.ident.name.as_str();
        let new_type = match item.node {
            ast::ItemKind::Ty(ref ty, ref generics) => {
                // Can not yet convert generics.
                if generics.is_parameterized() {
                    return Ok(());
                }

                rust_to_c(&*ty, &name)?
            }
            _ => {
                return Err(Error {
                    level: Level::Bug,
                    span: Some(item.span),
                    message: "`parse_ty` called on wrong `Item_`".into(),
                });
            }
        };

        buffer.push_str(&format!("typedef {};\n\n", new_type));
        self.append_to_header(buffer, module, outputs)?;

        self.decls.insert(
            name.to_string(),
            header_name(module, &self.lib_name)?,
        );

        Ok(())
    }

    /// Convert a Rust enum into a C enum.
    ///
    /// The Rust enum must be marked with `#[repr(C)]` and must be public otherwise the function
    /// will abort.
    ///
    /// Bindgen will error if the enum if generic or if it contains non-unit variants.
    fn parse_enum(
        &mut self,
        item: &ast::Item,
        module: &[String],
        outputs: &mut Outputs,
    ) -> Result<(), Error> {
        let (repr_c, docs) = parse_attr(
            &item.attrs,
            check_repr_c,
            |attr| retrieve_docstring(attr, ""),
        );
        // If it's not #[repr(C)] then it can't be called from C.
        if !repr_c {
            return Ok(());
        }

        let mut buffer = String::new();
        buffer.push_str(&docs);

        let name = item.ident.name.as_str();
        buffer.push_str(&format!("typedef enum {} {{\n", name));
        if let ast::ItemKind::Enum(ref definition, ref generics) = item.node {
            if generics.is_parameterized() {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: "bindgen can not handle parameterized `#[repr(C)]` enums".into(),
                });
            }

            for var in &definition.variants {
                if !var.node.data.is_unit() {
                    return Err(Error {
                        level: Level::Error,
                        span: Some(var.span),
                        message: "bindgen can not handle `#[repr(C)]` enums with non-unit variants"
                            .into(),
                    });
                }

                let (_, docs) = parse_attr(
                    &var.node.attrs,
                    |_| true,
                    |attr| retrieve_docstring(attr, "\t"),
                );
                buffer.push_str(&docs);

                buffer.push_str(&format!("\t{}_{},\n", name, pprust::variant_to_string(var)));
            }
        } else {
            return Err(Error {
                level: Level::Bug,
                span: Some(item.span),
                message: "`parse_enum` called on wrong `Item_`".into(),
            });
        }

        buffer.push_str(&format!("}} {};\n\n", name));
        self.append_to_header(buffer, module, outputs)?;

        Ok(())
    }

    /// Convert a Rust struct into a C struct.
    ///
    /// The rust struct must be marked `#[repr(C)]` and must be public otherwise the function will
    /// abort.
    ///
    /// Bindgen will error if the struct is generic or if the struct is a unit or tuple struct.
    fn parse_struct(
        &mut self,
        item: &ast::Item,
        module: &[String],
        outputs: &mut Outputs,
    ) -> Result<(), Error> {
        let (repr_c, docs) = parse_attr(
            &item.attrs,
            check_repr_c,
            |attr| retrieve_docstring(attr, ""),
        );
        // If it's not #[repr(C)] then it can't be called from C.
        if !repr_c {
            return Ok(());
        }

        let mut buffer = String::new();
        buffer.push_str(&docs);

        let name = item.ident.name.as_str();
        buffer.push_str(&format!("typedef struct {}", name));

        if let ast::ItemKind::Struct(ref variants, ref generics) = item.node {
            if generics.is_parameterized() {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: "bindgen can not handle parameterized `#[repr(C)]` structs".into(),
                });
            }

            if variants.is_struct() {
                buffer.push_str(" {\n");

                for field in variants.fields() {
                    let (_, docs) = parse_attr(
                        &field.attrs,
                        |_| true,
                        |attr| retrieve_docstring(attr, "\t"),
                    );
                    buffer.push_str(&docs);

                    let name = match field.ident {
                        Some(name) => name.name.as_str(),
                        None => unreachable!("a tuple struct snuck through"),
                    };

                    let ty = rust_to_c(&*field.ty, &name)?;
                    self.add_dependencies(module, &ty.1)?;
                    buffer.push_str(&format!("\t{};\n", ty));
                }

                buffer.push_str("}");
            } else if variants.is_tuple() && variants.fields().len() == 1 {
                // #[repr(C)] pub struct Foo(Bar);  =>  typedef struct Foo Foo;
            } else {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: "bindgen can not handle unit or tuple `#[repr(C)]` structs with >1 members"
                        .into(),
                });
            }
        } else {
            return Err(Error {
                level: Level::Bug,
                span: Some(item.span),
                message: "`parse_struct` called on wrong `Item_`".into(),
            });
        }

        buffer.push_str(&format!(" {};\n\n", name));
        self.append_to_header(buffer, module, outputs)?;

        self.decls.insert(
            name.to_string(),
            header_name(module, &self.lib_name)?,
        );

        Ok(())
    }

    /// Convert a Rust function declaration into a C function declaration.
    ///
    /// The function declaration must be marked `#[no_mangle]` and have a C ABI otherwise the
    /// function will abort.
    ///
    /// If the declaration is generic or diverges then bindgen will error.
    fn parse_fn(
        &mut self,
        item: &ast::Item,
        module: &[String],
        outputs: &mut Outputs,
    ) -> Result<(), Error> {
        let (no_mangle, docs) = parse_attr(&item.attrs, check_no_mangle, |attr| {
            retrieve_docstring(attr, "")
        });
        // If it's not #[no_mangle] then it can't be called from C.
        if !no_mangle {
            return Ok(());
        }

        let name = item.ident.name.as_str();

        if let ast::ItemKind::Fn(ref fn_decl, _, _, abi, ref generics, _) = item.node {
            match abi {
                // If it doesn't have a C ABI it can't be called from C.
                Abi::C | Abi::Cdecl | Abi::Stdcall | Abi::Fastcall | Abi::System => {}
                _ => return Ok(()),
            }

            if generics.is_parameterized() {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: "bindgen can not handle parameterized extern functions".into(),
                });
            }

            self.transform_native_fn(
                &*fn_decl,
                &docs,
                &format!("{}", name),
                module,
                outputs,
            )?;

            Ok(())
        } else {
            Err(Error {
                level: Level::Bug,
                span: Some(item.span),
                message: "`parse_fn` called on wrong `Item_`".into(),
            })
        }
    }

    fn finalise_output(&mut self, outputs: &mut Outputs) -> Result<(), Error> {
        let mut depgraph = Graph::<String, String>::new();
        let nodes_map: HashMap<String, _> = outputs
            .keys()
            .map(|m| (m.clone(), depgraph.add_node(m.clone())))
            .collect();
        let node_ids_map: HashMap<_, String> = nodes_map
            .iter()
            .map(|(k, v)| (v.clone(), k.clone()))
            .collect();
        let mut edges = BTreeSet::new();

        // Wrap modules with common includes
        for (header_name, value) in outputs.iter_mut() {
            let code = format!("#include <stdint.h>\n#include <stdbool.h>\n\n{}", value);

            *value = wrap_guard(&wrap_extern(&code), header_name);

            // Building a graph of dependencies
            if let Some(module_deps) = self.deps.get(header_name) {
                for dep in module_deps {
                    if let Some(mod_name) = self.decls.get(dep) {
                        let pred = mod_name.to_string();
                        let succ = header_name.to_string();
                        if pred == succ {
                            continue;
                        }
                        let _ = edges.insert((nodes_map[&pred], nodes_map[&succ]));
                    }
                }
            }
        }

        // Topologically sort dependencies
        depgraph.extend_with_edges(&edges);

        let mut module_includes = String::new();
        let sorted_deps = unwrap!(algo::toposort(&depgraph, None));

        // Generate a top-level header
        for node_id in sorted_deps {
            let header_name = &node_ids_map[&node_id];

            module_includes.push_str(&format!("#include \"{}\"\n", header_name));
        }

        outputs.insert(From::from(format!("{}.h", self.lib_name)), module_includes);

        Ok(())
    }
}

/// Turn a Rust type with an associated name or type into a C type.
pub fn rust_to_c(ty: &ast::Ty, assoc: &str) -> Result<CTypeNamed, Error> {
    match ty.node {
        // Function pointers make life an absolute pain here.
        ast::TyKind::BareFn(ref bare_fn) => Ok(CTypeNamed(
            Default::default(),
            fn_ptr_to_c(bare_fn, ty.span, assoc)?,
        )),
        // All other types just have a name associated with them.
        _ => Ok(CTypeNamed(assoc.to_string(), anon_rust_to_c(ty)?)),
    }
}

/// Turn a Rust type into a C type.
fn anon_rust_to_c(ty: &ast::Ty) -> Result<CType, Error> {
    match ty.node {
        // Function pointers should not be in this function.
        ast::TyKind::BareFn(..) => Err(Error {
            level: Level::Error,
            span: Some(ty.span),
            message: "C function pointers must have a name or function declaration associated with them"
                .into(),
        }),
        // Fixed-length arrays, converted into pointers.
        ast::TyKind::Array(ref ty, _) => {
            Ok(CType::Ptr(Box::new(anon_rust_to_c(ty)?), CPtrType::Const))
        }
        // Standard pointers.
        ast::TyKind::Ptr(ref ptr) => ptr_to_c(ptr),
        // Plain old types.
        ast::TyKind::Path(None, ref path) => path_to_c(path),
        // Possibly void, likely not.
        _ => {
            let new_type = print::pprust::ty_to_string(ty);
            if new_type == "()" {
                // Ok("void".into())
                Ok(CType::Void)
            } else {
                Err(Error {
                    level: Level::Error,
                    span: Some(ty.span),
                    message: format!("bindgen can not handle the type `{}`", new_type),
                })
            }
        }
    }
}

/// Turn a Rust pointer (*mut or *const) into the correct C form.
fn ptr_to_c(ty: &ast::MutTy) -> Result<CType, Error> {
    let new_type = anon_rust_to_c(&ty.ty)?;
    let const_spec = match ty.mutbl {
        // *const T
        ast::Mutability::Immutable => CPtrType::Const,
        // *mut T
        ast::Mutability::Mutable => CPtrType::Mutable,
    };

    Ok(CType::Ptr(Box::new(new_type), const_spec))
}

/// Turn a Rust function pointer into a C function pointer.
///
/// Rust function pointers are of the form
///
/// ```ignore
/// fn(arg1: Ty1, ...) -> RetTy
/// ```
///
/// C function pointers are of the form
///
/// ```C
/// RetTy (*inner)(Ty1 arg1, ...)
/// ```
///
/// where `inner` could either be a name or the rest of a function declaration.
fn fn_ptr_to_c(fn_ty: &ast::BareFnTy, fn_span: codemap::Span, inner: &str) -> Result<CType, Error> {
    if !fn_ty.lifetimes.is_empty() {
        return Err(Error {
            level: Level::Error,
            span: Some(fn_span),
            message: "bindgen can not handle lifetimes".into(),
        });
    }

    let fn_decl: &ast::FnDecl = &*fn_ty.decl;

    let args = if fn_decl.inputs.is_empty() {
        // No args
        vec![]
    } else {
        let mut args = vec![];
        for arg in &fn_decl.inputs {
            let arg_name = print::pprust::pat_to_string(&*arg.pat);
            let arg_type = rust_to_c(&*arg.ty, &arg_name)?;
            args.push(arg_type);
        }
        args
    };

    let output_type = &fn_decl.output;

    let return_type = match *output_type {
        ast::FunctionRetTy::Ty(ref ty) if ty.node == ast::TyKind::Never => {
            return Err(Error {
                level: Level::Error,
                span: Some(ty.span),
                message: "panics across a C boundary are naughty!".into(),
            });
        }
        ast::FunctionRetTy::Default(..) => CType::Void,
        ast::FunctionRetTy::Ty(ref ty) => anon_rust_to_c(&*ty)?,
    };

    Ok(CType::FnDecl {
        inner: inner.to_string(),
        args,
        return_type: Box::new(return_type),
    })
}

/// Convert a Rust path type (e.g. `my_mod::MyType`) to a C type.
///
/// Types hidden behind modules are almost certainly custom types (which wouldn't work) except
/// types in `libc` which we special case.
fn path_to_c(path: &ast::Path) -> Result<CType, Error> {
    if path.segments.is_empty() {
        return Err(Error {
            level: Level::Bug,
            span: Some(path.span),
            message: "invalid type".into(),
        });
    }

    // Types in modules, `my_mod::MyType`.
    if path.segments.len() > 1 {
        let (ty, module) = path.segments.split_last().expect(
            "already checked that there were at least two elements",
        );
        let ty: &str = &ty.identifier.name.as_str();
        let mut segments = Vec::with_capacity(module.len());
        for segment in module {
            segments.push(String::from(&*segment.identifier.name.as_str()));
        }
        let module = segments.join("::");
        match &*module {
            "libc" => Ok(libc_ty_to_c(ty).into()),
            "std::os::raw" => Ok(osraw_ty_to_c(ty).into()),
            _ => Err(Error {
                level: Level::Error,
                span: Some(path.span),
                message: "bindgen can not handle types in other modules (except `libc` and `std::os::raw`)"
                    .into(),
            }),
        }
    } else {
        Ok(
            rust_ty_to_c(&path.segments[0].identifier.name.as_str()).into(),
        )
    }
}

/// Convert a Rust type from `libc` into a C type.
///
/// Most map straight over but some have to be converted.
fn libc_ty_to_c(ty: &str) -> CType {
    match ty {
        "c_void" => CType::Void,
        "c_float" => CType::Native("float"),
        "c_double" => CType::Native("double"),
        "c_char" => CType::Native("char"),
        "c_schar" => CType::Native("signed char"),
        "c_uchar" => CType::Native("unsigned char"),
        "c_short" => CType::Native("short"),
        "c_ushort" => CType::Native("unsigned short"),
        "c_int" => CType::Native("int"),
        "c_uint" => CType::Native("unsigned int"),
        "c_long" => CType::Native("long"),
        "c_ulong" => CType::Native("unsigned long"),
        "c_longlong" => CType::Native("long long"),
        "c_ulonglong" => CType::Native("unsigned long long"),
        // All other types should map over to C.
        ty => CType::Mapping(ty.to_string()),
    }
}

/// Convert a Rust type from `std::os::raw` into a C type.
///
/// These mostly mirror the libc crate.
fn osraw_ty_to_c(ty: &str) -> CType {
    match ty {
        "c_void" => CType::Void,
        "c_char" => CType::Native("char"),
        "c_double" => CType::Native("double"),
        "c_float" => CType::Native("float"),
        "c_int" => CType::Native("int"),
        "c_long" => CType::Native("long"),
        "c_longlong" => CType::Native("long long"),
        "c_schar" => CType::Native("signed char"),
        "c_short" => CType::Native("short"),
        "c_uchar" => CType::Native("unsigned char"),
        "c_uint" => CType::Native("unsigned int"),
        "c_ulong" => CType::Native("unsigned long"),
        "c_ulonglong" => CType::Native("unsigned long long"),
        "c_ushort" => CType::Native("unsigned short"),
        // All other types should map over to C.
        ty => CType::Mapping(ty.to_string()),
    }
}

/// Convert any Rust type into C.
///
/// This includes user-defined types. We currently trust the user not to use types which we don't
/// know the structure of (like String).
fn rust_ty_to_c(ty: &str) -> CType {
    match ty {
        "()" => CType::Void,
        "f32" => CType::Native("float"),
        "f64" => CType::Native("double"),
        "i8" => CType::Native("int8_t"),
        "i16" => CType::Native("int16_t"),
        "i32" => CType::Native("int32_t"),
        "i64" => CType::Native("int64_t"),
        "isize" => CType::Native("intptr_t"),
        "u8" => CType::Native("uint8_t"),
        "u16" => CType::Native("uint16_t"),
        "u32" => CType::Native("uint32_t"),
        "u64" => CType::Native("uint64_t"),
        "usize" => CType::Native("uintptr_t"),
        "bool" => CType::Native("bool"),
        ty => libc_ty_to_c(ty),
    }
}


/// Wrap a block of code with an extern declaration.
fn wrap_extern(code: &str) -> String {
    format!(
        r#"
#ifdef __cplusplus
extern "C" {{
#endif

{}

#ifdef __cplusplus
}}
#endif
"#,
        code
    )
}

/// Wrap a block of code with an include-guard.
fn wrap_guard(code: &str, id: &str) -> String {
    format!(
        r"
#ifndef bindgen_{0}
#define bindgen_{0}

 {1}

#endif
",
        sanitise_id(id),
        code
    )
}

/// Transform a module name into a header name
fn header_name(module: &[String], lib_name: &str) -> Result<String, Error> {
    let mut module_name: Vec<String> = module.iter().cloned().collect::<_>();
    if module_name[0] == "ffi" {
        module_name[0] = lib_name.to_string();

        // Top-level module for a library - e.g. safe_app/safe_app.h
        if module_name.len() == 1 {
            module_name.push(lib_name.to_string());
        }
    }

    let header_name = format!("{}.h", module_name.join(&path::MAIN_SEPARATOR.to_string()));

    Ok(header_name)
}

/// Remove illegal characters from the identifier.
///
/// This is because macros names must be valid C identifiers. Note that the identifier will always
/// be concatenated onto `cheddar_generated_` so can start with a digit.
pub fn sanitise_id(id: &str) -> String {
    // `char.is_digit(36)` ensures `char` is in `[A-Za-z0-9]`
    id.chars()
        .filter(|ch| ch.is_digit(36) || *ch == '_')
        .collect()
}

#[derive(Debug, PartialEq)]
pub struct CTypeNamed(String, CType);

impl Display for CTypeNamed {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self.1 {
            // Function declarations don't need to be prefixed, so it's a
            // special case
            CType::FnDecl { .. } => write!(f, "{}", self.1),

            // For all other cases we add a type prefix
            _ => write!(f, "{} {}", self.1, self.0),
        }

    }
}

#[derive(Debug, PartialEq)]
pub enum CPtrType {
    Const,
    Mutable,
}

impl Display for CPtrType {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            CPtrType::Const => write!(f, " const"),
            CPtrType::Mutable => Ok(()),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum CType {
    Void,
    Mapping(String),
    Native(&'static str),
    Ptr(Box<CType>, CPtrType),
    FnDecl {
        inner: String,
        args: Vec<CTypeNamed>,
        return_type: Box<CType>,
    },
}

impl CType {
    fn dependencies(&self) -> Vec<String> {
        match *self {
            CType::FnDecl {
                ref args,
                ref return_type,
                ..
            } => {
                return_type
                    .dependencies()
                    .iter()
                    .cloned()
                    .chain(args.iter().flat_map(
                        |&CTypeNamed(_, ref cty)| cty.dependencies(),
                    ))
                    .collect()
            }
            CType::Ptr(ref cty, _) => cty.dependencies(),
            CType::Mapping(ref mapping) => vec![mapping.clone()],
            _ => Default::default(),
        }
    }
}

impl Display for CType {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            CType::Void => write!(f, "void"),
            CType::Mapping(ref s) => write!(f, "{}", s),
            CType::Native(ref s) => write!(f, "{}", s),
            CType::Ptr(ref cty, ref ptrty) => write!(f, "{}{}*", cty, ptrty),
            CType::FnDecl {
                ref inner,
                ref args,
                ref return_type,
            } => {
                write!(
                    f,
                    "{} (*{})({})",
                    return_type,
                    inner,
                    if args.is_empty() {
                        "void".to_string()
                    } else {
                        args.iter()
                            .map(|cty| format!("{}", cty))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::CType;
    use syntax::ast;

    #[test]
    fn sanitise_id() {
        assert!(super::sanitise_id("") == "");
        assert!(super::sanitise_id("!@£$%^&*()_+") == "_");
        // https://github.com/Sean1708/rusty-cheddar/issues/29
        assert!(super::sanitise_id("filename.h") == "filenameh");
    }

    fn ty(source: &str) -> ast::Ty {
        let sess = ::syntax::parse::ParseSess::new();
        let result = {
            let mut parser =
                ::syntax::parse::new_parser_from_source_str(&sess, "".into(), source.into());
            parser.parse_ty()
        };

        match result {
            Ok(p) => (*p).clone(),
            _ => {
                panic!(
                    "internal testing error: could not parse type from {:?}",
                    source
                )
            }
        }
    }

    #[test]
    fn pure_rust_types() {
        let type_map = [
            ("()", CType::Void),
            ("f32", CType::Native("float")),
            ("f64", CType::Native("double")),
            ("i8", CType::Native("int8_t")),
            ("i16", CType::Native("int16_t")),
            ("i32", CType::Native("int32_t")),
            ("i64", CType::Native("int64_t")),
            ("isize", CType::Native("intptr_t")),
            ("u8", CType::Native("uint8_t")),
            ("u16", CType::Native("uint16_t")),
            ("u32", CType::Native("uint32_t")),
            ("u64", CType::Native("uint64_t")),
            ("usize", CType::Native("uintptr_t")),
        ];

        let name = "gabriel";

        for &(rust_type, ref correct_c_type) in &type_map {
            let parsed_c_type = super::anon_rust_to_c(&ty(rust_type)).expect(&format!(
                "error while parsing {:?} with no name",
                rust_type
            ));
            assert_eq!(&parsed_c_type, correct_c_type);

            let parsed_c_type = super::rust_to_c(&ty(rust_type), name).expect(&format!(
                "error while parsing {:?} with name {:?}",
                rust_type,
                name
            ));
            assert_eq!(
                format!("{}", parsed_c_type),
                format!("{} {}", correct_c_type, name)
            );
        }
    }

    #[test]
    fn libc_types() {
        let type_map = [
            ("libc::c_void", "void"),
            ("libc::c_float", "float"),
            ("libc::c_double", "double"),
            ("libc::c_char", "char"),
            ("libc::c_schar", "signed char"),
            ("libc::c_uchar", "unsigned char"),
            ("libc::c_short", "short"),
            ("libc::c_ushort", "unsigned short"),
            ("libc::c_int", "int"),
            ("libc::c_uint", "unsigned int"),
            ("libc::c_long", "long"),
            ("libc::c_ulong", "unsigned long"),
            ("libc::c_longlong", "long long"),
            ("libc::c_ulonglong", "unsigned long long"),
            // Some other common ones.
            ("libc::size_t", "size_t"),
            ("libc::dirent", "dirent"),
            ("libc::FILE", "FILE"),
        ];

        let name = "lucifer";

        for &(rust_type, correct_c_type) in &type_map {
            let parsed_c_type = super::anon_rust_to_c(&ty(rust_type)).expect(&format!(
                "error while parsing {:?} with no name",
                rust_type
            ));
            assert_eq!(format!("{}", parsed_c_type), correct_c_type);

            let parsed_c_type = super::rust_to_c(&ty(rust_type), name).expect(&format!(
                "error while parsing {:?} with name {:?}",
                rust_type,
                name
            ));
            assert_eq!(
                format!("{}", parsed_c_type),
                format!("{} {}", correct_c_type, name)
            );
        }
    }

    #[test]
    fn const_pointers() {
        let name = "maalik";

        let source = "*const u8";
        let parsed_type = super::anon_rust_to_c(&ty(source)).expect(&format!(
            "error while parsing {:?} with no name",
            source
        ));
        assert_eq!(format!("{}", parsed_type), "uint8_t const*");

        let source = "*const ()";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(format!("{}", parsed_type), format!("void const* {}", name));

        let source = "*const *const f64";
        let parsed_type = super::anon_rust_to_c(&ty(source)).expect(&format!(
            "error while parsing {:?} with no name",
            source
        ));
        assert_eq!(format!("{}", parsed_type), "double const* const*");

        let source = "*const *const i64";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(
            format!("{}", parsed_type),
            format!("int64_t const* const* {}", name)
        );
    }

    #[test]
    fn mut_pointers() {
        let name = "raphael";

        let source = "*mut u16";
        let parsed_type = super::anon_rust_to_c(&ty(source)).expect(&format!(
            "error while parsing {:?} with no name",
            source
        ));
        assert_eq!(format!("{}", parsed_type), "uint16_t*");

        let source = "*mut f32";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(format!("{}", parsed_type), format!("float* {}", name));

        let source = "*mut *mut *mut i32";
        let parsed_type = super::anon_rust_to_c(&ty(source)).expect(&format!(
            "error while parsing {:?} with no name",
            source
        ));
        assert_eq!(format!("{}", parsed_type), "int32_t***");

        let source = "*mut *mut i8";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(format!("{}", parsed_type), format!("int8_t** {}", name));
    }

    #[test]
    fn mixed_pointers() {
        let name = "samael";

        let source = "*const *mut *const bool";
        let parsed_type = super::anon_rust_to_c(&ty(source)).expect(&format!(
            "error while parsing {:?} with no name",
            source
        ));
        assert_eq!(format!("{}", parsed_type), "bool const** const*");

        let source = "*mut *mut *const libc::c_ulonglong";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(
            format!("{}", parsed_type),
            format!("unsigned long long const*** {}", name)
        );

        let source = "*const *mut *mut i8";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(
            format!("{}", parsed_type),
            format!("int8_t** const* {}", name)
        );
    }

    #[test]
    fn function_pointers() {
        let name = "sariel";

        let source = "fn(a: bool)";
        let parsed_type = super::anon_rust_to_c(&ty(source));
        assert!(
            parsed_type.is_err(),
            "C function pointers should have an inner or name associated"
        );

        // let source = "fn(a: i8) -> f64";
        // let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
        //     "error while parsing {:?} with name {:?}",
        //     source,
        //     name
        // ));
        // assert!(parsed_type.is_none(), "parsed a non-C function pointer");

        let source = "extern fn(hi: libc::c_int) -> libc::c_double";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(
            format!("{}", parsed_type),
            format!("double (*{})(int hi)", name)
        );
    }

    #[test]
    fn paths() {
        let name = "zachariel";

        let source = "MyType";
        let parsed_type = super::anon_rust_to_c(&ty(source)).expect(&format!(
            "error while parsing {:?} with no name",
            source
        ));
        assert_eq!(format!("{}", parsed_type), "MyType");

        let source = "SomeType";
        let parsed_type = super::rust_to_c(&ty(source), name).expect(&format!(
            "error while parsing {:?} with name {:?}",
            source,
            name
        ));
        assert_eq!(format!("{}", parsed_type), format!("SomeType {}", name));

        let source = "my_mod::MyType";
        let parsed_type = super::anon_rust_to_c(&ty(source));
        assert!(
            parsed_type.is_err(),
            "can't use a multi-segment path which isn't `libc`"
        );

        let source = "some_mod::SomeType";
        let parsed_type = super::rust_to_c(&ty(source), name);
        assert!(
            parsed_type.is_err(),
            "can't use a multi-segment path which isn't `libc`"
        );
    }
}
