extern crate bytes;
extern crate bytesize;
extern crate chrono;
extern crate chrono_humanize;
extern crate futures;
extern crate futures_cpupool;
extern crate futures_fs;
#[macro_use]
extern crate horrorshow;
extern crate hyper;
extern crate pretty_env_logger;
extern crate structopt;
#[macro_use]
extern crate structopt_derive;
extern crate tar;
extern crate tokio_core;
extern crate url;
extern crate walkdir;

use bytes::Bytes;
use bytesize::ByteSize;
use chrono_humanize::HumanTime;
use chrono::{DateTime, Local};
use futures_cpupool::CpuPool;
use futures_fs::FsPool;
use futures::{future, stream, Future, Sink, Stream};
use futures::sink::Wait;
use futures::sync::mpsc::{self, Sender};
use horrorshow::prelude::*;
use horrorshow::helper::doctype;
use hyper::{Get, Post, StatusCode};
use hyper::header::{Charset, ContentDisposition, ContentLength, ContentType, DispositionParam,
                    DispositionType, Location};
use hyper::mime::{Mime, TEXT_HTML_UTF_8};
use hyper::server::{Http, Request, Response, Service};
use std::error;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
use tar::Builder;
use tokio_core::reactor::{Core, Handle};
use url::{form_urlencoded, percent_encoding};
use walkdir::{DirEntry, WalkDir};

struct Pipe {
    dest: Wait<Sender<Bytes>>,
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.dest.send(Bytes::from(buf)) {
            Ok(_) => Ok(buf.len()),
            Err(e) => Err(io::Error::new(ErrorKind::Interrupted, e)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.dest.flush() {
            Ok(_) => Ok(()),
            Err(e) => Err(io::Error::new(ErrorKind::Interrupted, e)),
        }
    }
}

struct Archiver<W: Write> {
    builder: Builder<W>,
}

impl<W: Write> Archiver<W> {
    fn new(builder: Builder<W>) -> Self {
        Self { builder }
    }
}

impl<W: Write> Archiver<W> {
    fn write_entry(&mut self, root: &Path, entry: &DirEntry) -> Result<(), Box<error::Error>>
    where
        W: Write,
    {
        let relative_path = entry.path().strip_prefix(&root)?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            self.builder.append_dir(&relative_path, &entry.path())?;
        } else {
            let mut file = File::open(&entry.path())?;
            self.builder.append_file(&relative_path, &mut file)?;
        }

        Ok(())
    }

    fn add_to_archive<P, Q>(&mut self, root: P, entry: Q)
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
        W: Write,
    {
        let walkdir = WalkDir::new(root.as_ref().join(&entry));
        let entries = walkdir.into_iter().filter_entry(|e| !Self::is_hidden(e));
        for e in entries {
            match e {
                Ok(e) => if let Err(e) = self.write_entry(root.as_ref(), &e) {
                    println!("{}", e);
                },
                Err(e) => println!("{}", e),
            }
        }
    }

    fn is_hidden(entry: &DirEntry) -> bool {
        entry
            .file_name()
            .to_str()
            .map_or(true, |s| s.starts_with('.'))
    }
}

#[derive(Clone, Debug, StructOpt)]
struct Options {
    #[structopt(short = "r", long = "root", help = "Root path", default_value = ".",
                parse(from_os_str))]
    root: PathBuf,
}

struct Server {
    options: Options,
    handle: Handle,
    fs_pool: FsPool,
    pool: CpuPool,
}

impl Service for Server {
    type Request = Request;
    type Response = Response<Box<Stream<Item = Bytes, Error = Self::Error> + Send>>;
    type Error = hyper::Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: Request) -> Self::Future {
        type Body = Box<Stream<Item = Bytes, Error = hyper::Error> + Send>;

        let root = self.options.root.as_path();
        let path_ = percent_encoding::percent_decode(req.path().as_bytes())
            .decode_utf8()
            .unwrap()
            .into_owned();
        let path = root.join(Path::new(&path_).strip_prefix("/").unwrap());
        match *req.method() {
            Get => {
                let metadata = fs::metadata(&path);
                if metadata.is_err() {
                    let response = Response::new().with_status(StatusCode::InternalServerError);
                    Box::new(future::ok(response))
                } else {
                    let metadata = metadata.unwrap();
                    if metadata.is_dir() {
                        if !path_.ends_with('/') {
                            let response = Response::new()
                                .with_status(StatusCode::Found)
                                .with_header(Location::new(path_.to_string() + "/"));
                            Box::new(future::ok(response))
                        } else {
                            let entries = get_dir_index(&path).unwrap();
                            let bytes = Bytes::from(render_index(entries));
                            let len = bytes.len() as u64;
                            let body = Box::new(stream::once(Ok(bytes))) as Body;
                            let response = Response::new()
                                .with_status(StatusCode::Ok)
                                .with_header(ContentType(TEXT_HTML_UTF_8))
                                .with_header(ContentLength(len))
                                .with_body(body);

                            Box::new(future::ok(response))
                        }
                    } else {
                        let body = Box::new(self.fs_pool.read(path).map_err(|e| e.into())) as Body;
                        let response = Response::new()
                            .with_status(StatusCode::Ok)
                            .with_header(ContentLength(metadata.len()))
                            .with_body(body);

                        Box::new(future::ok(response))
                    }
                }
            }
            Post => {
                let pool = self.pool.clone();
                let handle = self.handle.clone();
                let b = req.body().concat2().and_then(move |b| {
                    let mut files = Vec::new();
                    for (name, value) in form_urlencoded::parse(&b) {
                        if name == "s" {
                            let value = percent_encoding::percent_decode(value.as_bytes())
                                .decode_utf8()
                                .unwrap()
                                .into_owned();
                            files.push(value)
                        } else {
                            let response = Response::new().with_status(StatusCode::BadRequest);
                            return Ok(response);
                        }
                    }

                    let archive_name = match files.len() {
                        0 => {
                            files.push(String::from("."));
                            (path_.clone() + ".tar").as_bytes().to_vec()
                        }
                        1 => (files[0].clone() + ".tar").as_bytes().to_vec(),
                        _ => b"archive.tar".to_vec(),
                    };

                    let (tx, rx) = mpsc::channel(10);
                    let f = pool.spawn_fn(move || {
                        let pipe = Pipe { dest: tx.wait() };

                        let mut archiver = Archiver::new(Builder::new(pipe));
                        for file in files {
                            archiver.add_to_archive(&path, &file);
                        }

                        future::ok::<_, ()>(())
                    });
                    handle.spawn(f);

                    Ok(
                        Response::new()
                            .with_header(ContentDisposition {
                                disposition: DispositionType::Attachment,
                                parameters: vec![
                                    DispositionParam::Filename(
                                        Charset::Iso_8859_1,
                                        None,
                                        archive_name,
                                    ),
                                ],
                            })
                            .with_header(ContentType("application/x-tar".parse::<Mime>().unwrap()))
                            .with_body(Box::new(rx.map_err(|_| hyper::Error::Incomplete)) as Body),
                    )
                });
                Box::new(b)
            }
            _ => {
                let response = Response::new().with_status(StatusCode::MethodNotAllowed);
                Box::new(future::ok(response))
            }
        }
    }
}

fn main() {
    pretty_env_logger::init().unwrap();

    let options = Options::from_args();
    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 3010);

    let fs_pool = FsPool::new(4);
    let cpu_pool = CpuPool::new_num_cpus();
    let handle1 = handle.clone();
    let server = Http::new()
        .serve_addr_handle(&addr, &handle, move || {
            Ok(Server {
                options: options.clone(),
                handle: handle1.clone(),
                fs_pool: fs_pool.clone(),
                pool: cpu_pool.clone(),
            })
        })
        .unwrap();

    let handle1 = handle.clone();
    handle.spawn(
        server
            .for_each(move |conn| {
                handle1.spawn(conn.map(|_| ()).map_err(|err| println!("error: {:?}", err)));
                Ok(())
            })
            .map_err(|_| ()),
    );

    core.run(future::empty::<(), ()>()).unwrap();
}

#[derive(Debug)]
struct ShareEntry {
    name: OsString,
    is_dir: bool,
    size: u64,
    date: DateTime<Local>,
}

fn render_index(entries: Vec<ShareEntry>) -> String {
    (html! {
        : doctype::HTML;
        html {
            head {
                script { : Raw(include_str!("../assets/player.js")) }
                style { : Raw(include_str!("../assets/style.css")); }
            }
            body {
                form(method="POST") {
                    table(class="view") {
                        colgroup {
                            col(class="selected");
                            col(class="name");
                            col(class="size");
                            col(class="date");
                        }
                        tr(class="header") {
                            th;
                            th { : Raw("Name") }
                            th { : Raw("Size") }
                            th { : Raw("Last modified") }
                        }
                        tr { td; td { a(href=Raw("..")) { : Raw("..") } } td; td; }
                        @ for ShareEntry { mut name, is_dir, size, date } in entries {
                            |tmpl| {
                                let mut display_name = name;
                                if is_dir {
                                    display_name.push("/");
                                }
                                let link = percent_encoding::percent_encode(
                                    display_name.as_bytes(),
                                    percent_encoding::DEFAULT_ENCODE_SET,
                                ).to_string();
                                let name = display_name.to_string_lossy().into_owned();
                                let size = ByteSize::b(size as usize).to_string(false);
                                tmpl << html! {
                                    tr {
                                        td { input(name="s", value=Raw(&link), type="checkbox") }
                                        td { a(href=Raw(&link)) { : name } }
                                        @ if is_dir { td } else { td { : Raw(size) } }
                                        td { : Raw(HumanTime::from(date).to_string()) }
                                    }
                                }
                            }
                        }
                    }
                    input(type="submit", value="Download");
                }
            }
        }
    }).into_string()
        .unwrap()
}

fn get_dir_index(path: &Path) -> io::Result<Vec<ShareEntry>> {
    let mut entries = Vec::new();
    for file in fs::read_dir(&path)? {
        let entry = file?;
        let metadata = entry.metadata()?;
        let name = entry.file_name();
        if !is_hidden(&name) {
            entries.push(ShareEntry {
                name,
                is_dir: metadata.is_dir(),
                size: metadata.len(),
                date: metadata.modified()?.into(),
            });
        }
    }
    entries.sort_by_key(|e| (!e.is_dir, e.date));

    Ok(entries)
}

fn is_hidden(path: &OsStr) -> bool {
    path.as_bytes().starts_with(b".")
}
