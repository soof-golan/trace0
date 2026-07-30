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

    /// `slot` namespaces a child's packet sequences away from every other
    /// process merged into the same trace. The JSON format needs no such thing:
    /// its events carry a pid outright.
    pub fn open(self, path: &str, slot: u32) -> io::Result<Box<dyn Exporter>> {
        Ok(match self {
            Format::Json => Box::new(JsonExporter::create(path)?),
            Format::Protobuf => Box::new(ProtoExporter::create(path)?.with_slot(slot)),
        })
    }
}
