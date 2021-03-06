#![feature(proc_macro_internals)]
#![feature(proc_macro_span)]
#![feature(proc_macro_diagnostic)]
//extern crate dylib;
extern crate sharedlib;
extern crate libloading;
extern crate goblin;
extern crate proc_macro;
#[macro_use]
extern crate serde_derive;

//use dylib::DynamicLibrary;
use goblin::mach::Mach;
use goblin::Object;
use macro_expansion::{ExpansionResult, ExpansionTask};
use proc_macro::bridge::client::ProcMacro;
use proc_macro::bridge::server::SameThread;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use sharedlib::{Lib, Data, Symbol};
use libloading::Library;

pub mod macro_expansion;
mod rustc_server;

static NEW_REGISTRAR_SYMBOL: &str = "__rustc_proc_macro_decls_";
static _OLD_REGISTRAR_SYMBOL: &str = "__rustc_derive_registrar_";

const EXEC_STRATEGY: SameThread = SameThread;

fn parse_string(code: &str) -> Option<proc_macro2::TokenStream> {
    syn::parse_str(code).ok()
}

fn read_bytes(file: &Path) -> Option<Vec<u8>> {
    let mut fd = File::open(file).ok()?;
    let mut buffer = Vec::new();
    fd.read_to_end(&mut buffer).ok()?;

    Some(buffer)
}

fn get_symbols_from_lib(file: &Path) -> Option<Vec<String>> {
    let buffer = read_bytes(file)?;
    let object = Object::parse(&buffer).ok()?;

    return match object {
        Object::Elf(elf) => {
            let symbols = elf.dynstrtab.to_vec().ok()?;
            let names = symbols.iter().map(|s| s.to_string()).collect();

            Some(names)
        }

        Object::PE(pe) => {
            let symbol_names = pe.exports.iter()
                .flat_map(|s| s.name)
                .map(|n| n.to_string())
                .collect();
            Some(symbol_names)
        }

        Object::Mach(mach) => match mach {
            Mach::Binary(binary) => {
                let exports = binary.exports().ok()?;
                let names = exports.iter().map(|s| s.name.clone()).collect();

                Some(names)
            }

            Mach::Fat(_) => None,
        },

        Object::Archive(_) | Object::Unknown(_) => None,
    };
}

fn is_derive_registrar_symbol(symbol: &str) -> bool {
    symbol.contains(NEW_REGISTRAR_SYMBOL)
}

fn find_registrar_symbol(file: &Path) -> Option<String> {
    let symbols = get_symbols_from_lib(file)?;

    symbols
        .iter()
        .find(|s| is_derive_registrar_symbol(s))
        .map(|s| s.to_string())
}

/// Loads dynamic library in platform dependent manner.
///
/// For unix, you have to use RTLD_DEEPBIND flag to escape problems described
/// [here](https://github.com/fedochet/rust-proc-macro-panic-inside-panic-expample)
/// and [here](https://github.com/rust-lang/rust/issues/60593).
///
/// Usage of RTLD_DEEPBIND is suggested by @edwin0cheng
/// [here](https://github.com/fedochet/rust-proc-macro-panic-inside-panic-expample/issues/1)
///
/// It seems that on Windows that behaviour is default, so we do nothing in that case.
#[cfg(windows)]
fn load_library(file: &Path) -> Result<Library, std::io::Error> {
    Library::new(file)
}

#[cfg(unix)]
fn load_library(file: &Path) -> Result<Library, std::io::Error> {
    use std::os::raw::c_int;
    use libloading::os::unix::Library as UnixLibrary;

    const RTLD_NOW: c_int = 0x00002;
    const RTLD_DEEPBIND: c_int = 0x00008;

    UnixLibrary::open(Some(file), RTLD_NOW | RTLD_DEEPBIND).map(|lib| lib.into())
}

struct ProcMacroLibraryLibloading {
    lib: Library,
    exported_macros: Vec<ProcMacro>,
}

impl ProcMacroLibraryLibloading {
    fn open(file: &Path) -> Result<Self, String> {
        let symbol_name = find_registrar_symbol(file)
            .ok_or(format!("Cannot find registrar symbol in file {:?}", file))?;

        let lib = load_library(file).map_err(|e| e.to_string())?;

        let exported_macros = {
            let macros: libloading::Symbol<&&[ProcMacro]> = unsafe { lib.get(symbol_name.as_bytes()) }
                .map_err(|e| e.to_string())?;

            macros.to_vec()
        };

        Ok(ProcMacroLibraryLibloading {
            lib,
            exported_macros,
        })
    }
}

struct ProcMacroLibrarySharedLib {
    lib: Lib,
    exported_macros: Vec<ProcMacro>,
}

impl ProcMacroLibrarySharedLib {
    fn open(file: &Path) -> Result<Self, String> {
        let symbol_name = find_registrar_symbol(file)
            .ok_or(format!("Cannot find registrar symbol in file {:?}", file))?;

        let lib = unsafe { Lib::new(file) }.map_err(|e| e.to_string())?;

        let exported_macros = {
            // data already implies reference
            let macros: Data<&[ProcMacro]> = unsafe { lib.find_data(&symbol_name) }
                .map_err(|e| e.to_string())?;

            unsafe { *macros.get() }.to_vec()
        };

        Ok(ProcMacroLibrarySharedLib {
            lib,
            exported_macros,
        })
    }
}

///// This struct keeps opened dynamic library and macros, from it, together.
/////
///// As long as lib is alive, exported_macros are safe to use.
//struct ProcMacroLibraryDylib {
//    lib: DynamicLibrary,
//    exported_macros: Vec<ProcMacro>,
//}
//
//impl ProcMacroLibraryDylib {
//    fn open(file: &PathBuf) -> Result<ProcMacroLibraryDylib, String> {
//        let symbol_name = find_registrar_symbol(file)
//            .ok_or(format!("Cannot find registrar symbol in file {:?}", file))?;
//
//        let lib = DynamicLibrary::open(Some(file))?;
//
//        let registrar = unsafe {
//            let symbol = lib.symbol(&symbol_name)?;
//            std::mem::transmute::<*mut u8, &&[ProcMacro]>(symbol)
//        };
//
//        let exported_macros: Vec<ProcMacro> = registrar.to_vec();
//
//        Ok(ProcMacroLibraryDylib {
//            lib,
//            exported_macros,
//        })
//    }
//}

type ProcMacroLibraryImpl = ProcMacroLibraryLibloading;

pub struct Expander {
    libs: Vec<ProcMacroLibraryImpl>,
}

impl Expander {
    pub fn new<P: AsRef<Path>>(libs_paths: &[P]) -> Result<Expander, String> {
        let mut libs = vec![];

        for lib in libs_paths {
            /* Some libraries for dynamic loading require canonicalized path (even when it is
            already absolute
            */
            let lib = lib.as_ref().canonicalize().expect(
                &format!("Cannot canonicalize {:?}", lib.as_ref())
            );

            let library = ProcMacroLibraryImpl::open(&lib)?;
            libs.push(library)
        }

        Ok(Expander { libs })
    }

    pub fn expand(
        &self,
        macro_name: &str,
        macro_body: &str,
        attributes: Option<&String>,
    ) -> Result<String, proc_macro::bridge::PanicMessage> {
        let parsed_body = parse_string(macro_body).expect(
            &format!("Error while parsing this code: '{}'", macro_body)
        );

        let parsed_attributes = attributes.map_or(proc_macro2::TokenStream::new(), |attr| {
            parse_string(attr).expect(
                &format!("Error while parsing this code: '{}'", attr)
            )
        });

        for lib in &self.libs {
            for proc_macro in &lib.exported_macros {
                match proc_macro {
                    ProcMacro::CustomDerive {
                        trait_name, client, ..
                    } if *trait_name == macro_name => {
                        let res = client.run(
                            &EXEC_STRATEGY,
                            rustc_server::Rustc::default(),
                            parsed_body,
                        );

                        return res.map(|token_stream| token_stream.to_string());
                    }

                    ProcMacro::Bang { name, client } if *name == macro_name => {
                        let res = client.run(
                            &EXEC_STRATEGY,
                            rustc_server::Rustc::default(),
                            parsed_body,
                        );

                        return res.map(|token_stream| token_stream.to_string());
                    }

                    ProcMacro::Attr { name, client } if *name == macro_name => {
                        let res = client.run(
                            &EXEC_STRATEGY,
                            rustc_server::Rustc::default(),
                            parsed_attributes,
                            parsed_body,
                        );

                        return res.map(|token_stream| token_stream.to_string());
                    }

                    _ => {
                        continue;
                    }
                }
            }
        }

        Err(proc_macro::bridge::PanicMessage::String(
            "Nothing to expand".to_string(),
        ))
    }
}

pub fn expand_task(task: &ExpansionTask) -> ExpansionResult {
    let expander = Expander::new(&task.libs).expect(
        &format!("Cannot expand with provided libraries: ${:?}", &task.libs)
    );

    let result = match expander.expand(&task.macro_name, &task.macro_body, task.attributes.as_ref()) {
        Ok(expansion) => ExpansionResult::Success { expansion },

        Err(msg) => {
            let reason = format!(
                "Cannot perform expansion for {}: error {:?}!",
                &task.macro_name,
                msg.as_str()
            );

            ExpansionResult::Error { reason }
        }
    };

    result
}
