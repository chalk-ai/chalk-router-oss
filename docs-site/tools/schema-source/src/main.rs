//! Syntax-only source extraction: no router build, credentials, or private dependency needed.
use quote::ToTokens;
use serde_json::{json, Value};
use std::{env, fs};
use syn::{spanned::Spanned, visit::Visit, Expr, Item};

fn tokens<T: ToTokens>(v: &T) -> String {
    v.to_token_stream().to_string()
}
fn attrs(a: &[syn::Attribute]) -> Vec<String> {
    a.iter()
        .filter(|a| a.path().is_ident("serde"))
        .map(tokens)
        .collect()
}
fn comments(a: &[syn::Attribute]) -> String {
    a.iter()
        .filter_map(|a| match &a.meta {
            syn::Meta::NameValue(v) if a.path().is_ident("doc") => match &v.value {
                Expr::Lit(v) => match &v.lit {
                    syn::Lit::Str(s) => Some(s.value().trim().to_string()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn fields(f: &syn::Fields) -> Vec<Value> {
    f.iter()
        .enumerate()
        .map(|(i, f)| {
            json!({
                "name": f.ident.as_ref().map(ToString::to_string).unwrap_or_else(|| i.to_string()),
                "type": tokens(&f.ty), "serde": attrs(&f.attrs), "comment": comments(&f.attrs)
            })
        })
        .collect()
}
struct Routes(Vec<Value>);
impl<'ast> Visit<'ast> for Routes {
    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if e.method == "route" && e.args.len() == 2 {
            if let Expr::Lit(p) = &e.args[0] {
                if let syn::Lit::Str(p) = &p.lit {
                    fn collect(e: &Expr, path: &str, out: &mut Vec<Value>) {
                        let (method, args) = match e {
                            Expr::Call(c) => (tokens(&c.func), &c.args),
                            Expr::MethodCall(c) => {
                                collect(&c.receiver, path, out);
                                (c.method.to_string(), &c.args)
                            }
                            _ => panic!("Unrecognized route expression: {}", tokens(e)),
                        };
                        assert!(
                            ["get", "post", "delete", "put", "patch", "head", "options"]
                                .contains(&method.as_str()),
                            "Unknown method {method}"
                        );
                        out.push(json!({"path": path, "method": method.to_uppercase(), "handler": tokens(&args[0]).replace(' ', ""), "line": e.span().start().line}));
                    }
                    collect(&e.args[1], &p.value(), &mut self.0);
                }
            }
        }
        syn::visit::visit_expr_method_call(self, e);
    }
}
fn main() {
    if env::args().nth(1).as_deref() == Some("--protoc") {
        println!(
            "{}",
            json!({"binary":protoc_bin_vendored::protoc_bin_path().unwrap(),"include":protoc_bin_vendored::include_path().unwrap()})
        );
        return;
    }
    let root = env::args().nth(1).expect("repository root");
    let paths = [
        "src/types.rs",
        "src/responses.rs",
        "src/decisions.rs",
        "src/translation/decisions_openai.rs",
        "src/route_grid.rs",
        "src/capabilities.rs",
        "src/handlers/traces.rs",
        "src/handlers/models.rs",
        "src/handlers/responses.rs",
    ];
    let mut definitions = vec![];
    for path in paths {
        let source = fs::read_to_string(format!("{root}/{path}")).unwrap();
        let ast = syn::parse_file(&source).unwrap();
        let lines: Vec<_> = source.lines().collect();
        for item in ast.items {
            let (name, attributes, field_list, variants) = match &item {
                Item::Struct(s) => (s.ident.to_string(), &s.attrs, fields(&s.fields), vec![]),
                Item::Enum(e) => (e.ident.to_string(), &e.attrs, vec![], e.variants.iter().map(|v| json!({"name":v.ident.to_string(),"serde":attrs(&v.attrs),"fields":fields(&v.fields),"comment":comments(&v.attrs)})).collect()),
                _ => continue,
            };
            // Include only serialized types, including private response DTOs; exclude implementation state.
            if !attributes.iter().any(|a| {
                a.path().is_ident("derive")
                    && (tokens(a).contains("Serialize") || tokens(a).contains("Deserialize"))
            }) {
                continue;
            }
            let start = item.span().start().line;
            let end = item.span().end().line;
            definitions.push(json!({"name":name,"file":path,"line":start,"source":lines[start-1..end].join("\n"),"serde":attrs(attributes),"comment":comments(attributes),"fields":field_list,"variants":variants}));
        }
    }
    let main =
        syn::parse_file(&fs::read_to_string(format!("{root}/src/main.rs")).unwrap()).unwrap();
    let mut routes = Routes(vec![]);
    routes.visit_file(&main);
    routes
        .0
        .sort_by_key(|v| format!("{} {}", v["path"], v["method"]));
    let grpc_source = fs::read_to_string(format!("{root}/src/grpc/router.rs")).unwrap();
    let grpc_ast = syn::parse_file(&grpc_source).unwrap();
    let registered_services: Vec<String> = grpc_ast
        .items
        .iter()
        .find_map(|item| {
            let Item::Const(c) = item else {
                return None;
            };
            if c.ident != "GRPC_SERVICES" {
                return None;
            }
            let Expr::Reference(r) = c.expr.as_ref() else {
                panic!("Expected service registry reference");
            };
            let Expr::Array(a) = r.expr.as_ref() else {
                panic!("Expected service registry array");
            };
            Some(
                a.elems
                    .iter()
                    .map(|e| {
                        let Expr::Lit(l) = e else {
                            panic!("Expected service name literal");
                        };
                        let syn::Lit::Str(s) = &l.lit else {
                            panic!("Expected service name string");
                        };
                        s.value()
                    })
                    .collect(),
            )
        })
        .expect("GRPC_SERVICES registry");
    let descriptor_path = env::args().nth(2).expect("descriptor file");
    let bytes = fs::read(descriptor_path).unwrap();
    let pool = prost_reflect::DescriptorPool::decode(bytes.as_slice()).unwrap();
    let descriptor = prost_reflect::DynamicMessage::decode(
        pool.get_message_by_name("google.protobuf.FileDescriptorSet")
            .unwrap(),
        bytes.as_slice(),
    )
    .unwrap();
    for entry in fs::read_dir(format!("{root}/docs-site/examples")).unwrap() {
        let path = entry.unwrap().path();
        if path.to_string_lossy().ends_with(".grpc.json") {
            let example: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            let name = example["type"].as_str().unwrap();
            let body = example["body"].to_string();
            prost_reflect::DynamicMessage::deserialize(
                pool.get_message_by_name(name)
                    .expect("example message type"),
                &mut serde_json::Deserializer::from_str(&body),
            )
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
    }
    println!(
        "{}",
        json!({"types":definitions,"routes":routes.0,"descriptor":descriptor,"registeredServices":registered_services})
    );
}
