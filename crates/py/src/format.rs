use std::io;
use trace0_core::Exporter;
use trace0_json::JsonExporter;
use trace0_proto::ProtoExporter;

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Json,
    Protobuf,
}

impl Format {
    pub fn parse(s: &str) -> io::Result<Self> {
        match s {
            "json" => Ok(Format::Json),
            "protobuf" | "proto" | "pb" => Ok(Format::Protobuf),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown format: {other}"),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Protobuf => "protobuf",
        }
    }

    pub fn open(self, path: &str) -> io::Result<Box<dyn Exporter>> {
        Ok(match self {
            Format::Json => Box::new(JsonExporter::create(path)?),
            Format::Protobuf => Box::new(ProtoExporter::create(path)?),
        })
    }
}

pub fn make_exporter(format: &str, path: &str) -> io::Result<Box<dyn Exporter>> {
    Format::parse(format)?.open(path)
}
