// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(unused_imports,dead_code,unused_variables)]

use ::compiler::{
    gcc,
    Cacheable,
    CompilerArguments,
    write_temp_file,
};
use compiler::c::{CCompilerImpl, CCompilerKind, ParsedArguments};
use futures::future::{self, Future};
use futures_cpupool::CpuPool;
use mock_command::{
    CommandCreator,
    CommandCreatorSync,
    RunCommand,
};
use std::ffi::OsString;
use std::fs::File;
use std::io::{
    self,
    Write,
};
use std::path::Path;
use std::process;
use util::{run_input_output, OsStrExt};

use errors::*;

/// A unit struct on which to implement `CCompilerImpl`.
#[derive(Clone, Debug)]
pub struct Clang;

impl CCompilerImpl for Clang {
    fn kind(&self) -> CCompilerKind { CCompilerKind::Clang }
    fn parse_arguments(&self,
                       arguments: &[OsString],
                       cwd: &Path) -> CompilerArguments<ParsedArguments>
    {
        gcc::parse_arguments(arguments, cwd, argument_takes_value)
    }

    fn preprocess<T>(&self,
                     creator: &T,
                     executable: &Path,
                     parsed_args: &ParsedArguments,
                     cwd: &Path,
                     env_vars: &[(OsString, OsString)],
                     pool: &CpuPool)
                     -> SFuture<process::Output> where T: CommandCreatorSync
    {
        gcc::preprocess(creator, executable, parsed_args, cwd, env_vars, pool)
    }

    fn compile<T>(&self,
                  creator: &T,
                  executable: &Path,
                  preprocessor_result: process::Output,
                  parsed_args: &ParsedArguments,
                  cwd: &Path,
                  env_vars: &[(OsString, OsString)],
                  pool: &CpuPool)
                  -> SFuture<(Cacheable, process::Output)>
        where T: CommandCreatorSync
    {
        compile(creator, executable, preprocessor_result, parsed_args, cwd, env_vars, pool)
    }
}

/// Arguments that take a value that aren't in `gcc::ARGS_WITH_VALUE`.
const ARGS_WITH_VALUE: &'static [&'static str] = &[
    "-B",
    "-target",
    "-Xclang",
    "--serialize-diagnostics",
];

/// Return true if `arg` is a clang commandline argument that takes a value.
pub fn argument_takes_value(arg: &str) -> bool {
    gcc::ARGS_WITH_VALUE.contains(&arg) || ARGS_WITH_VALUE.contains(&arg)
}

fn compile<T>(creator: &T,
              executable: &Path,
              preprocessor_result: process::Output,
              parsed_args: &ParsedArguments,
              cwd: &Path,
              env_vars: &[(OsString, OsString)],
              pool: &CpuPool)
              -> SFuture<(Cacheable, process::Output)>
    where T: CommandCreatorSync,
{
    trace!("compile");
    // Clang needs a temporary file for compilation, otherwise debug info
    // doesn't have a reference to the input file.
    let write = {
        let filename = match Path::new(&parsed_args.input).file_name() {
            Some(name) => name,
            None => return f_err("Missing input filename"),
        };
  
        Box::new(
            write_temp_file(pool, filename.as_ref(), preprocessor_result.stdout.clone())
                 .and_then(move |(tempdir, input)| {
                     match input.into_os_string().into_string() {
                         Ok(p) => future::ok((None, vec!(p), Some(tempdir))),
                         Err(_) => future::err("Failed to write input file".into()),
                     }
                 })
)
    };

    gcc::compile(creator, executable, preprocessor_result, parsed_args, cwd, env_vars, pool, Some(write))
}

#[cfg(test)]
mod test {
    use compiler::*;
    use compiler::gcc;
    use futures::Future;
    use futures_cpupool::CpuPool;
    use mock_command::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use super::*;
    use test::utils::*;

    fn _parse_arguments(arguments: &[String]) -> CompilerArguments<ParsedArguments> {
        let arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        Clang.parse_arguments(&arguments, ".".as_ref())
    }

    macro_rules! parses {
        ( $( $s:expr ),* ) => {
            match _parse_arguments(&[ $( $s.to_string(), )* ]) {
                CompilerArguments::Ok(a) => a,
                o @ _ => panic!("Got unexpected parse result: {:?}", o),
            }
        }
    }


    #[test]
    fn test_parse_arguments_simple() {
        let a = parses!("-c", "foo.c", "-o", "foo.o");
        assert_eq!(Some("foo.c"), a.input.to_str());
        assert_eq!("c", a.extension);
        assert_map_contains!(a.outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, a.outputs.len());
        assert!(a.preprocessor_args.is_empty());
        assert!(a.common_args.is_empty());
    }

    #[test]
    fn test_parse_arguments_values() {
        let a = parses!("-c", "foo.cxx", "-arch", "xyz", "-fabc","-I", "include", "-o", "foo.o", "-include", "file");
        assert_eq!(Some("foo.cxx"), a.input.to_str());
        assert_eq!("cxx", a.extension);
        assert_map_contains!(a.outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, a.outputs.len());
        assert_eq!(ovec!["-include", "file"], a.preprocessor_args);
        assert_eq!(ovec!["-arch", "xyz", "-fabc", "-I", "include"], a.common_args);
    }

    #[test]
    fn test_parse_arguments_others() {
        parses!("-c", "foo.c", "-Xclang", "-load", "-Xclang", "moz-check", "-o", "foo.o");
        parses!("-c", "foo.c", "-B", "somewhere", "-o", "foo.o");
        parses!("-c", "foo.c", "-target", "x86_64-apple-darwin11", "-o", "foo.o");
    }

    #[test]
    fn test_compile_simple() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        let parsed_args = ParsedArguments {
            input: "foo.c".into(),
            extension: "c".into(),
            depfile: None,
            outputs: vec![("obj", "foo.o".into())].into_iter().collect(),
            preprocessor_args: vec!(),
            common_args: vec!(),
            msvc_show_includes: false,
        };
        let compiler = &f.bins[0];
        // Compiler invocation.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "", "")));
        let (cacheable, _) = compile(&creator,
                                     &compiler,
                                     empty_output(),
                                     &parsed_args,
                                     f.tempdir.path(),
                                     &[],
                                     &pool).wait().unwrap();
        assert_eq!(Cacheable::Yes, cacheable);
        // Ensure that we ran all processes.
        assert_eq!(0, creator.lock().unwrap().children.len());
    }

    #[test]
    fn test_compile_werror_fails() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        let parsed_args = ParsedArguments {
            input: "foo.c".into(),
            extension: "c".into(),
            depfile: None,
            outputs: vec![("obj", "foo.o".into())].into_iter().collect(),
            preprocessor_args: vec!(),
            common_args: ovec!("-c", "-o", "foo.o", "-Werror=blah", "foo.c"),
            msvc_show_includes: false,
        };
        let compiler = &f.bins[0];
        // First compiler invocation fails.
        next_command(&creator, Ok(MockChild::new(exit_status(1), "", "")));
        // Second compiler invocation succeeds.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "", "")));
        let (cacheable, output) = compile(&creator,
                                          &compiler,
                                          empty_output(),
                                          &parsed_args,
                                          f.tempdir.path(),
                                          &[],
                                          &pool).wait().unwrap();
        assert_eq!(Cacheable::Yes, cacheable);
        assert_eq!(exit_status(0), output.status);
        // Ensure that we ran all processes.
        assert_eq!(0, creator.lock().unwrap().children.len());
    }
}
