// Copyright (c) 2017-present PyO3 Project and Contributors
//! Code generation for the function that initializes a python module and adds classes and function.

use crate::args;
use crate::method;
use crate::py_method;
use crate::utils;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn;

/// Generates the function that is called by the python interpreter to initialize the native
/// module
pub fn py3_init(fnname: &syn::Ident, name: &syn::Ident, doc: syn::Lit) -> TokenStream {
    let cb_name = syn::Ident::new(&format!("PyInit_{}", name), Span::call_site());

    quote! {
        #[no_mangle]
        #[allow(non_snake_case)]
        /// This autogenerated function is called by the python interpreter when importing
        /// the module.
        pub unsafe extern "C" fn #cb_name() -> *mut ::pyo3::ffi::PyObject {
            ::pyo3::derive_utils::make_module(concat!(stringify!(#name), "\0"), #doc, #fnname)
        }
    }
}

pub fn py2_init(fnname: &syn::Ident, name: &syn::Ident, doc: syn::Lit) -> TokenStream {
    let cb_name = syn::Ident::new(&format!("init{}", name), Span::call_site());

    quote! {
        #[no_mangle]
        #[allow(non_snake_case)]
        pub unsafe extern "C" fn #cb_name() {
            ::pyo3::derive_utils::make_module(concat!(stringify!(#name), "\0"), #doc, #fnname)
        }
    }
}

/// Finds and takes care of the #[pyfn(...)] in `#[pymodule]`
pub fn process_functions_in_module(func: &mut syn::ItemFn) {
    let mut stmts: Vec<syn::Stmt> = Vec::new();

    for stmt in func.block.stmts.iter_mut() {
        if let syn::Stmt::Item(syn::Item::Fn(ref mut func)) = stmt {
            if let Some((module_name, python_name, pyfn_attrs)) =
                extract_pyfn_attrs(&mut func.attrs)
            {
                let function_to_python = add_fn_to_module(func, &python_name, pyfn_attrs);
                let function_wrapper_ident = function_wrapper_ident(&func.ident);
                let item: syn::ItemFn = syn::parse_quote! {
                    fn block_wrapper() {
                        #function_to_python
                        #module_name.add_wrapped(&#function_wrapper_ident)?;
                    }
                };
                stmts.extend(item.block.stmts.into_iter());
            }
        };
        stmts.push(stmt.clone());
    }

    func.block.stmts = stmts;
}

/// Transforms a rust fn arg parsed with syn into a method::FnArg
fn wrap_fn_argument<'a>(input: &'a syn::FnArg, name: &'a syn::Ident) -> Option<method::FnArg<'a>> {
    match input {
        &syn::FnArg::SelfRef(_) | &syn::FnArg::SelfValue(_) => None,
        &syn::FnArg::Captured(ref cap) => {
            let (mutability, by_ref, ident) = match cap.pat {
                syn::Pat::Ident(ref patid) => (&patid.mutability, &patid.by_ref, &patid.ident),
                _ => panic!("unsupported argument: {:?}", cap.pat),
            };

            let py = match cap.ty {
                syn::Type::Path(ref typath) => typath
                    .path
                    .segments
                    .last()
                    .map(|seg| seg.value().ident == "Python")
                    .unwrap_or(false),
                _ => false,
            };

            let opt = method::check_arg_ty_and_optional(&name, &cap.ty);
            Some(method::FnArg {
                name: ident,
                mutability,
                by_ref,
                ty: &cap.ty,
                optional: opt,
                py,
                reference: method::is_ref(&name, &cap.ty),
            })
        }
        &syn::FnArg::Ignored(_) => panic!("ignored argument: {:?}", name),
        &syn::FnArg::Inferred(_) => panic!("inferred argument: {:?}", name),
    }
}

/// Extracts the data from the #[pyfn(...)] attribute of a function
fn extract_pyfn_attrs(
    attrs: &mut Vec<syn::Attribute>,
) -> Option<(syn::Ident, syn::Ident, Vec<args::Argument>)> {
    let mut new_attrs = Vec::new();
    let mut fnname = None;
    let mut modname = None;
    let mut fn_attrs = Vec::new();

    for attr in attrs.iter() {
        match attr.interpret_meta() {
            Some(syn::Meta::List(ref list)) if list.ident == "pyfn" => {
                let meta: Vec<_> = list.nested.iter().cloned().collect();
                if meta.len() >= 2 {
                    // read module name
                    match meta[0] {
                        syn::NestedMeta::Meta(syn::Meta::Word(ref ident)) => {
                            modname = Some(ident.clone())
                        }
                        _ => panic!("The first parameter of pyfn must be a MetaItem"),
                    }
                    // read Python fonction name
                    match meta[1] {
                        syn::NestedMeta::Literal(syn::Lit::Str(ref lits)) => {
                            fnname = Some(syn::parse_str(&lits.value()).unwrap());
                        }
                        _ => panic!("The second parameter of pyfn must be a Literal"),
                    }
                    // Read additional arguments
                    if list.nested.len() >= 3 {
                        fn_attrs = args::parse_arguments(&meta[2..meta.len()]);
                    }
                } else {
                    panic!("can not parse 'pyfn' params {:?}", attr);
                }
            }
            _ => new_attrs.push(attr.clone()),
        }
    }

    *attrs = new_attrs;
    Some((modname?, fnname?, fn_attrs))
}

/// Coordinates the naming of a the add-function-to-python-module function
fn function_wrapper_ident(name: &syn::Ident) -> syn::Ident {
    // Make sure this ident matches the one of wrap_pyfunction
    // The trim_start_matches("r#") is for https://github.com/dtolnay/syn/issues/478
    syn::Ident::new(
        &format!(
            "__pyo3_get_function_{}",
            name.to_string().trim_start_matches("r#")
        ),
        Span::call_site(),
    )
}

/// Generates python wrapper over a function that allows adding it to a python module as a python
/// function
pub fn add_fn_to_module(
    func: &syn::ItemFn,
    python_name: &syn::Ident,
    pyfn_attrs: Vec<args::Argument>,
) -> TokenStream {
    let mut arguments = Vec::new();

    for input in func.decl.inputs.iter() {
        if let Some(fn_arg) = wrap_fn_argument(input, &func.ident) {
            arguments.push(fn_arg);
        }
    }

    let ty = method::get_return_info(&func.decl.output);

    let spec = method::FnSpec {
        tp: method::FnType::Fn,
        attrs: pyfn_attrs,
        args: arguments,
        output: ty,
    };

    let function_wrapper_ident = function_wrapper_ident(&func.ident);

    let wrapper = function_c_wrapper(&func.ident, &spec);
    let doc = utils::get_doc(&func.attrs, true);

    let tokens = quote! {
        fn #function_wrapper_ident(py: ::pyo3::Python) -> ::pyo3::PyObject {
            #wrapper

            let _def = ::pyo3::class::PyMethodDef {
                ml_name: stringify!(#python_name),
                ml_meth: ::pyo3::class::PyMethodType::PyCFunctionWithKeywords(__wrap),
                ml_flags: ::pyo3::ffi::METH_VARARGS | ::pyo3::ffi::METH_KEYWORDS,
                ml_doc: #doc,
            };

            let function = unsafe {
                ::pyo3::PyObject::from_owned_ptr_or_panic(
                    py,
                    ::pyo3::ffi::PyCFunction_New(
                        Box::into_raw(Box::new(_def.as_method_def())),
                        ::std::ptr::null_mut()
                    )
                )
            };

            function
        }
    };

    tokens
}

/// Generate static function wrapper (PyCFunction, PyCFunctionWithKeywords)
fn function_c_wrapper(name: &syn::Ident, spec: &method::FnSpec<'_>) -> TokenStream {
    let names: Vec<syn::Ident> = spec
        .args
        .iter()
        .enumerate()
        .map(|item| {
            if item.1.py {
                syn::Ident::new("_py", Span::call_site())
            } else {
                syn::Ident::new(&format!("arg{}", item.0), Span::call_site())
            }
        })
        .collect();
    let cb = quote! {
        ::pyo3::ReturnTypeIntoPyResult::return_type_into_py_result(#name(#(#names),*))
    };

    let body = py_method::impl_arg_params(spec, cb);
    let body_to_result = py_method::body_to_result(&body, spec);

    quote! {
        unsafe extern "C" fn __wrap(
            _slf: *mut ::pyo3::ffi::PyObject,
            _args: *mut ::pyo3::ffi::PyObject,
            _kwargs: *mut ::pyo3::ffi::PyObject) -> *mut ::pyo3::ffi::PyObject
        {
            const _LOCATION: &'static str = concat!(stringify!(#name), "()");

            let _pool = ::pyo3::GILPool::new();
            let _py = ::pyo3::Python::assume_gil_acquired();
            let _args = _py.from_borrowed_ptr::<::pyo3::types::PyTuple>(_args);
            let _kwargs: Option<&::pyo3::types::PyDict> = _py.from_borrowed_ptr_or_opt(_kwargs);

            #body_to_result
            ::pyo3::callback::cb_convert(
                ::pyo3::callback::PyObjectCallbackConverter, _py, _result)
        }
    }
}
