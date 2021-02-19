/*! Metal Shading Language (MSL) backend

## Binding model

Metal's bindings are flat per resource. Since there isn't an obvious mapping
from SPIR-V's descriptor sets, we require a separate mapping provided in the options.
This mapping may have one or more resource end points for each descriptor set + index
pair.

## Outputs

In Metal, built-in shader outputs can not be nested into structures within
the output struct. If there is a structure in the outputs, and it contains any built-ins,
we move them up to the root output structure that we define ourselves.
!*/

use crate::{
    arena::Handle,
    proc::{analyzer::Analysis, ResolveError},
    FastHashMap,
};
use std::{
    io::{Error as IoError, Write},
    string::FromUtf8Error,
};

mod keywords;
mod writer;

pub use writer::Writer;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BindTarget {
    pub buffer: Option<u8>,
    pub texture: Option<u8>,
    pub sampler: Option<u8>,
    pub mutable: bool,
}

#[derive(Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct BindSource {
    pub stage: crate::ShaderStage,
    pub group: u32,
    pub binding: u32,
}

pub type BindingMap = FastHashMap<BindSource, BindTarget>;

enum ResolvedBinding {
    BuiltIn(crate::BuiltIn),
    Attribute(u32),
    Color(u32),
    User { prefix: &'static str, index: u32 },
    Resource(BindTarget),
}

// Note: some of these should be removed in favor of proper IR validation.

#[derive(Debug)]
pub enum Error {
    IO(IoError),
    Utf8(FromUtf8Error),
    Type(ResolveError),
    MissingBindTarget(BindSource),
    InvalidImageAccess(crate::StorageAccess),
    BadName(String),
    UnimplementedBindTarget(BindTarget),
    UnsupportedCompose(Handle<crate::Type>),
    UnsupportedBinaryOp(crate::BinaryOperator),
    UnexpectedSampleLevel(crate::SampleLevel),
    UnsupportedCall(String),
    UnsupportedDynamicArrayLength,
    FeatureNotImplemented(String),
    /// The source IR is not valid.
    Validation,
}

impl From<IoError> for Error {
    fn from(e: IoError) -> Self {
        Error::IO(e)
    }
}

impl From<FromUtf8Error> for Error {
    fn from(e: FromUtf8Error) -> Self {
        Error::Utf8(e)
    }
}

impl From<ResolveError> for Error {
    fn from(e: ResolveError) -> Self {
        Error::Type(e)
    }
}

#[derive(Clone, Copy, Debug)]
enum LocationMode {
    VertexInput,
    FragmentOutput,
    Intermediate,
    Uniform,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// (Major, Minor) target version of the Metal Shading Language.
    pub lang_version: (u8, u8),
    /// Make it possible to link different stages via SPIRV-Cross.
    pub spirv_cross_compatibility: bool,
    /// Binding model mapping to Metal.
    pub binding_map: BindingMap,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            lang_version: (1, 0),
            spirv_cross_compatibility: false,
            binding_map: BindingMap::default(),
        }
    }
}

impl Options {
    fn resolve_binding(
        &self,
        stage: crate::ShaderStage,
        var: &crate::GlobalVariable,
        mode: LocationMode,
    ) -> Result<ResolvedBinding, Error> {
        match var.binding {
            Some(crate::Binding::BuiltIn(built_in)) => Ok(ResolvedBinding::BuiltIn(built_in)),
            Some(crate::Binding::Location(index)) => match mode {
                LocationMode::VertexInput => Ok(ResolvedBinding::Attribute(index)),
                LocationMode::FragmentOutput => Ok(ResolvedBinding::Color(index)),
                LocationMode::Intermediate => Ok(ResolvedBinding::User {
                    prefix: if self.spirv_cross_compatibility {
                        "locn"
                    } else {
                        "loc"
                    },
                    index,
                }),
                LocationMode::Uniform => {
                    log::error!(
                        "Unexpected Binding::Location({}) for the Uniform mode",
                        index
                    );
                    Err(Error::Validation)
                }
            },
            Some(crate::Binding::Resource { group, binding }) => {
                let source = BindSource {
                    stage,
                    group,
                    binding,
                };
                Ok(ResolvedBinding::Resource(self.binding_map
                    .get(&source)
                    .cloned()
                    .unwrap_or_default()
                ))//.ok_or(Error::MissingBindTarget(source))
            }
            None => {
                log::error!("Missing binding for {:?}", var.name);
                Err(Error::Validation)
            }
        }
    }
}

impl ResolvedBinding {
    fn try_fmt<W: Write>(&self, out: &mut W) -> Result<(), Error> {
        match *self {
            ResolvedBinding::BuiltIn(built_in) => {
                use crate::BuiltIn as Bi;
                let name = match built_in {
                    // vertex
                    Bi::BaseInstance => "base_instance",
                    Bi::BaseVertex => "base_vertex",
                    Bi::ClipDistance => "clip_distance",
                    Bi::InstanceIndex => "instance_id",
                    Bi::PointSize => "point_size",
                    Bi::Position => "position",
                    Bi::VertexIndex => "vertex_id",
                    // fragment
                    Bi::FragCoord => "position",
                    Bi::FragDepth => "depth(any)",
                    Bi::FrontFacing => "front_facing",
                    Bi::SampleIndex => "sample_id",
                    Bi::SampleMaskIn => "sample_mask",
                    Bi::SampleMaskOut => "sample_mask",
                    // compute
                    Bi::GlobalInvocationId => "thread_position_in_grid",
                    Bi::LocalInvocationId => "thread_position_in_threadgroup",
                    Bi::LocalInvocationIndex => "thread_index_in_threadgroup",
                    Bi::WorkGroupId => "threadgroup_position_in_grid",
                    Bi::WorkGroupSize => "dispatch_threads_per_threadgroup",
                };
                Ok(write!(out, "{}", name)?)
            }
            ResolvedBinding::Attribute(index) => Ok(write!(out, "attribute({})", index)?),
            ResolvedBinding::Color(index) => Ok(write!(out, "color({})", index)?),
            ResolvedBinding::User { prefix, index } => {
                Ok(write!(out, "user({}{})", prefix, index)?)
            }
            ResolvedBinding::Resource(ref target) => {
                if let Some(id) = target.buffer {
                    Ok(write!(out, "buffer({})", id)?)
                } else if let Some(id) = target.texture {
                    Ok(write!(out, "texture({})", id)?)
                } else if let Some(id) = target.sampler {
                    Ok(write!(out, "sampler({})", id)?)
                } else {
                    Ok(write!(out, "unknown")?)
                    //Err(Error::UnimplementedBindTarget(target.clone()))
                }
            }
        }
    }

    fn try_fmt_decorated<W: Write>(&self, out: &mut W, terminator: &str) -> Result<(), Error> {
        write!(out, " [[")?;
        self.try_fmt(out)?;
        write!(out, "]]")?;
        write!(out, "{}", terminator)?;
        Ok(())
    }
}

/// Information about a translated module that is required
/// for the use of the result.
pub struct TranslationInfo {
    /// Mapping of the entry point names. Each item in the array
    /// corresponds to an entry point in `module.entry_points.iter()`.
    pub entry_point_names: Vec<String>,
}

pub fn write_string(
    module: &crate::Module,
    analysis: &Analysis,
    options: &Options,
) -> Result<(String, TranslationInfo), Error> {
    let mut w = writer::Writer::new(Vec::new());
    let info = w.write(module, analysis, options)?;
    let string = String::from_utf8(w.finish())?;
    Ok((string, info))
}
