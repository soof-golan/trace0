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
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "json" => Ok(Format::Json),
            "protobuf" | "proto" | "pb" => Ok(Format::Protobuf),
            other => Err(format!("unknown format: {other}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Protobuf => "protobuf",
        }
    }

    pub fn open(self, path: &str, slot: u32, append: bool) -> io::Result<Box<dyn Exporter>> {
        Ok(match (self, append) {
            (Format::Json, false) => Box::new(JsonExporter::create(path)?),
            (Format::Json, true) => Box::new(JsonExporter::append(path)?),
            (Format::Protobuf, false) => Box::new(ProtoExporter::create(path)?.with_slot(slot)),
            (Format::Protobuf, true) => Box::new(ProtoExporter::append(path)?.with_slot(slot)),
        })
    }
}
