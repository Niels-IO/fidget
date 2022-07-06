use bytemuck::{Pod, Zeroable};
use indoc::formatdoc;
use std::collections::{BTreeMap, BTreeSet};

use crate::program::{Block, Choice, Config, Instruction, Program, RegIndex};

impl Choice {
    fn to_metal(self) -> &'static str {
        match self {
            Self::Left => "LHS",
            Self::Right => "RHS",
            Self::Both => "BOTH",
        }
    }
}

/// A generated function.
///
/// The opening of the function is omitted but can be reconstructed
/// from the index, inputs, and outputs.
struct Function {
    index: usize,
    body: String,
    root: bool,
    /// Registers which are sourced externally to this block and unmodified
    inputs: BTreeSet<RegIndex>,
    /// Registers which are sourced externally to this block and modified
    outputs: BTreeSet<RegIndex>,
}

/// Shader mode
#[derive(Copy, Clone, Debug)]
pub enum Mode {
    Pixel,
    Interval,
}

impl Mode {
    fn function_prefix(&self) -> &str {
        match self {
            Mode::Pixel => "f",
            Mode::Interval => "i",
        }
    }

    fn vars_type(&self) -> &str {
        match self {
            Mode::Pixel => "const thread float*",
            Mode::Interval => "const thread float2*",
        }
    }
    fn local_type(&self) -> &str {
        match self {
            Mode::Pixel => "float",
            Mode::Interval => "float2",
        }
    }
    fn choice_type(&self) -> &str {
        match self {
            Mode::Pixel => "const device uint8_t*",
            Mode::Interval => "device uint8_t*",
        }
    }
}

impl Function {
    fn declaration(&self, mode: Mode) -> String {
        let mut out = String::new();
        out += &formatdoc!(
            "
            inline {} t_shape_{}(
                {} vars, {} choices",
            if self.root { mode.local_type() } else { "void" },
            self.index,
            mode.vars_type(),
            mode.choice_type(),
        );
        let mut first = true;
        for i in &self.inputs {
            if first {
                out += ",\n    ";
            } else {
                out += ", ";
            }
            first = false;
            out += &format!("const {} v{}", mode.local_type(), usize::from(*i));
        }
        let mut first = true;
        for i in &self.outputs {
            if first {
                out += ",\n    ";
            } else {
                out += ", ";
            }
            first = false;
            out +=
                &format!("thread {}& v{}", mode.local_type(), usize::from(*i));
        }
        out += "\n)";
        out
    }
    /// Generates text to call a function
    fn call(&self) -> String {
        let mut out = String::new();
        out += &format!("t_shape_{}(vars, choices", self.index);

        for i in &self.inputs {
            out += &format!(", v{}", usize::from(*i));
        }
        for i in &self.outputs {
            out += &format!(", v{}", usize::from(*i));
        }
        out += ");";
        out
    }
}

// Inject a `to_metal` function to `program::Instruction`
impl Instruction {
    fn to_metal(&self, mode: Mode) -> Option<String> {
        let out = usize::from(self.out_reg()?);
        let t = mode.function_prefix();
        Some(match self {
            Self::Var { var, .. } => {
                format!("v{out} = {t}_var(vars[{}]);", usize::from(*var))
            }
            Self::Const { value, .. } => {
                format!("v{out} = {t}_const({});", value)
            }
            Self::Mul { lhs, rhs, .. } | Self::Add { lhs, rhs, .. } => {
                format!(
                    "v{out} = {t}_{}(v{}, v{});",
                    self.name(),
                    usize::from(*lhs),
                    usize::from(*rhs)
                )
            }
            Self::Max {
                lhs, rhs, choice, ..
            }
            | Self::Min {
                lhs, rhs, choice, ..
            } => {
                let mut switch = formatdoc!(
                    "switch (choices[{}]) {{
                        case LHS: v{out} = v{}; break;
                        case RHS: v{out} = v{}; break;
                        default: ",
                    usize::from(*choice),
                    usize::from(*lhs),
                    usize::from(*rhs),
                );
                switch += &match mode {
                    Mode::Pixel => format!(
                        "v{out} = {t}_{}(v{}, v{}); break;",
                        self.name(),
                        usize::from(*lhs),
                        usize::from(*rhs)
                    ),
                    Mode::Interval => {
                        let a = usize::from(*lhs);
                        let b = usize::from(*lhs);
                        let choice = usize::from(*choice);
                        match self {
                            Self::Max { .. } => formatdoc!(
                                "
                            if (v{a}[0] > v{b}[1]) {{
                                choices[{choice}] = LHS;
                                v{out} = v{a};
                            }} else if (v{b}[0] > v{a}[1]) {{
                                choices[{choice}] = RHS;
                                v{out} = v{b};
                            }} else {{
                                v{out} = i_max(v{a}, v{b});
                            }}
                            "
                            ),
                            Self::Min { .. } => formatdoc!(
                                "
                            if (v{a}[1] < v{b}[0]) {{
                                choices[{choice}] = LHS;
                                v{out} = v{a};
                            }} else if (v{b}[1] < v{a}[0]) {{
                                choices[{choice}] = RHS;
                                v{out} = v{b};
                            }} else {{
                                v{out} = i_min(v{a}, v{b});
                            }}
                            "
                            ),
                            _ => unreachable!(),
                        }
                    }
                };
                switch + "}\n"
            }
            Self::Ln { reg, .. }
            | Self::Exp { reg, .. }
            | Self::Atan { reg, .. }
            | Self::Acos { reg, .. }
            | Self::Asin { reg, .. }
            | Self::Tan { reg, .. }
            | Self::Cos { reg, .. }
            | Self::Sin { reg, .. }
            | Self::Sqrt { reg, .. }
            | Self::Recip { reg, .. }
            | Self::Abs { reg, .. }
            | Self::Neg { reg, .. } => {
                format!("v{out} = {t}_{}(v{});", self.name(), usize::from(*reg))
            }
            Self::Cond(..) => return None,
        })
    }
}

impl Program {
    /// Converts the program to a Metal shader
    pub fn to_metal(&self, mode: Mode) -> String {
        let mut out = formatdoc!(
            "
            {}
            ",
            METAL_PRELUDE,
        );

        // Global map from block paths to (function index, body)
        let mut functions: BTreeMap<Vec<usize>, Function> = BTreeMap::new();
        self.to_metal_inner(&self.tape, mode, &mut vec![], &mut functions);

        out += "\n// Function definitions\n";
        for f in functions.values().rev() {
            out += &format!("{} {{\n{}}}\n", f.declaration(mode), f.body);
        }
        out += "\n";
        out += &formatdoc!(
            "
        // Root function
        inline {} t_eval({} vars,
                         {} choices)
        {{
            return t_shape_{}(vars, choices);
        }}
        ",
            mode.local_type(),
            mode.vars_type(),
            mode.choice_type(),
            functions.get(&vec![]).unwrap().index
        );
        out += &formatdoc!(
            "
            #define VAR_COUNT {}
            #define CHOICE_COUNT {}
            ",
            self.config().var_count,
            self.config().choice_count,
        );
        out += match mode {
            Mode::Interval => METAL_KERNEL_INTERVALS,
            Mode::Pixel => METAL_KERNEL_PIXELS,
        };
        out
    }

    fn to_metal_inner(
        &self,
        block: &Block,
        mode: Mode,
        path: &mut Vec<usize>,
        functions: &mut BTreeMap<Vec<usize>, Function>,
    ) {
        let mut first = true;
        let mut out = String::new();
        for r in block.locals.iter() {
            if first {
                out += &format!("    {} ", mode.local_type());
                first = false;
            } else {
                out += ", ";
            }
            out += &format!("v{}", usize::from(*r));
        }
        if !first {
            out += ";\n"
        }
        for (index, instruction) in block.tape.iter().enumerate() {
            if let Some(i) = instruction.to_metal(mode) {
                for line in i.lines() {
                    out += &format!("    {}\n", line);
                }
            } else if let Instruction::Cond(cond, next) = &instruction {
                // Recurse!
                path.push(index);
                self.to_metal_inner(next, mode, path, functions);
                let f = functions.get(path).unwrap();
                path.pop();

                // Write out the conditional, calling the inner function
                out += "    if (";
                if cond.len() > 1 {
                    let mut first = true;
                    for c in cond {
                        if first {
                            first = false;
                        } else {
                            out += " || ";
                        }
                        out += &format!(
                            "(choices[{}] & {})",
                            usize::from(c.0),
                            c.1.to_metal()
                        );
                    }
                } else {
                    out += &format!(
                        "choices[{}] & {}",
                        usize::from(cond[0].0),
                        cond[0].1.to_metal()
                    );
                }
                out += ") {\n        ";
                out += &f.call();
                out += "\n    }\n";
            } else {
                panic!("Could not get out register or Cond block");
            }
        }
        let i = functions.len();
        let is_root = path.is_empty();
        if is_root {
            out += &format!("    return v{};\n", usize::from(self.root));
        }
        functions.insert(
            path.clone(),
            Function {
                index: i,
                body: out,
                root: is_root,
                inputs: block.inputs.clone(),
                outputs: block.outputs.clone(),
            },
        );
    }
}

const METAL_PRELUDE: &str = r#"
// Prelude
#include <metal_stdlib>

#define RHS 1
#define LHS 2

// This must be kept in sync with the Rust `struct RenderConfig`!
struct RenderConfig {
    uint32_t image_size;
    uint32_t tile_size;
    uint32_t var_index_x;
    uint32_t var_index_y;
    uint32_t var_index_z;
};

// Floating-point math
inline float f_mul(const float a, const float b) {
    return a * b;
}
inline float f_add(const float a, const float b) {
    return a + b;
}

inline float f_min(const float a, const float b) {
    return metal::fmin(a, b);
}
inline float f_max(const float a, const float b) {
    return metal::fmax(a, b);
}
inline float f_neg(const float a) {
    return -a;
}
inline float f_sqrt(const float a) {
    return metal::sqrt(a);
}
inline float f_const(const float a) {
    return a;
}
inline float f_var(const float a) {
    return a;
}

// Interval math
inline float2 i_mul(const float2 a, const float2 b) {
    if (a[0] < 0.0f) {
        if (a[1] > 0.0f) {
            if (b[0] < 0.0f) {
                if (b[1] > 0.0f) { // M * M
                    return float2(metal::fmin(a[0] * b[1], a[1] * b[0]),
                                  metal::fmax(a[0] * b[0], a[1] * b[1]));
                } else { // M * N
                    return float2(a[1] * b[0], a[0] * b[0]);
                }
            } else {
                if (b[1] > 0.0f) { // M * P
                    return float2(a[0] * b[1], a[1] * b[1]);
                } else { // M * Z
                    return float2(0.0f, 0.0f);
                }
            }
        } else {
            if (b[0] < 0.0f) {
                if (b[1] > 0.0f) { // N * M
                    return float2(a[0] * b[1], a[0] * b[0]);
                } else { // N * N
                    return float2(a[1] * b[1], a[0] * b[0]);
                }
            } else {
                if (b[1] > 0.0f) { // N * P
                    return float2(a[0] * b[1], a[1] * b[0]);
                } else { // N * Z
                    return float2(0.0f, 0.0f);
                }
            }
        }
    } else {
        if (a[1] > 0.0f) {
            if (b[0] < 0.0f) {
                if (b[1] > 0.0f) { // P * M
                    return float2(a[1] * b[0], a[1] * b[1]);
                } else {// P * N
                    return float2(a[1] * b[0], a[0] * b[1]);
                }
            } else {
                if (b[1] > 0.0f) { // P * P
                    return float2(a[0] * b[0], a[1] * b[1]);
                } else {// P * Z
                    return float2(0.0f, 0.0f);
                }
            }
        } else { // Z * ?
            return float2(0.0f, 0.0f);
        }
    }
}
inline float2 i_add(const float2 a, const float2 b) {
    return a + b;
}
inline float2 i_min(const float2 a, const float2 b) {
    return metal::fmin(a, b);
}
inline float2 i_max(const float2 a, const float2 b) {
    return metal::fmax(a, b);
}
inline float2 i_neg(const float2 a) {
    return float2(-a[1], -a[0]);
}
inline float2 i_sqrt(const float2 a) {
    if (a[1] < 0.0) {
        return float2(-1e8, 1e8); // XXX
    } else if (a[0] <= 0.0) {
        return float2(0.0, metal::sqrt(a[1]));
    } else {
        return float2(metal::sqrt(a[0]), metal::sqrt(a[1]));
    }
}
inline float2 i_const(const float a) {
    return float2(a, a);
}
inline float2 i_var(const float2 a) {
    return a;
}
"#;

const METAL_KERNEL_INTERVALS: &str = r#"
kernel void main0({} vars [[buffer(0)]],
                  {} choices [[buffer(1)]],
                  device {}* result [[buffer(2)]],
                  uint index [[thread_position_in_grid]])
{
    result[index] = t_eval(&vars[index * VAR_COUNT],
                           &choices[index * CHOICE_COUNT]);
}
"#;

const METAL_KERNEL_PIXELS: &str = r#"
// This should be called with a 1D grid of size
//      ((cfg.image_size / cfg.tile_size) ** 2, 1, 1)
// and with a threadgroup size of
//      (cfg.tile_size ** 2, 1, 1).
kernel void main0(const device RenderConfig& cfg [[buffer(0)]],
                  const device uint32_t* tiles [[buffer(1)]],
                  const device uint8_t* choices [[buffer(2)]],
                  device uchar4* out [[buffer(3)]],
                  uint index [[thread_position_in_grid]])
{
    // Calculate the corner position of this tile, in pixels
    const uint32_t tile_index = index / (cfg.tile_size * cfg.tile_size);
    const uint32_t tile = tiles[tile_index];
    const uint2 tile_corner = cfg.tile_size * uint2(tile & 0xFFFF, tile >> 16);

    // Calculate the offset within the tile, again in pixels
    const uint32_t offset = index % (cfg.tile_size * cfg.tile_size);
    const uint2 tile_offset(offset % cfg.tile_size, offset / cfg.tile_size);

    // Absolute pixel position
    const uint2 pixel = tile_corner + tile_offset;

    // Early exit
    if (pixel.x > cfg.image_size || pixel.y > cfg.image_size) {
        //return;
    }

    // Image location (-1 to 1)
    const float2 pos = 1.0 - float2(pixel) / float2(cfg.image_size - 1) * 2.0;

    // Inject X and Y into local (thread) variables array
    float vars[VAR_COUNT];
    if (cfg.var_index_x < VAR_COUNT) {
        vars[cfg.var_index_x] = pos.x;
    }
    if (cfg.var_index_y < VAR_COUNT) {
        vars[cfg.var_index_y] = pos.y;
    }

    const float result =
        t_eval(vars, &choices[tile_index * CHOICE_COUNT]);

    const uint8_t v = result < 0.0 ? 0xFF : 0;

    out[pixel.x + pixel.y * cfg.image_size] = uchar4(v, v, v, 255);
}
"#;

// TODO:
/*
#define VC const device float* vars, const device uint8_t* choices
#define IF inline float
#define IV inline void
#define CF const float
#define TF thread float&
*/

////////////////////////////////////////////////////////////////////////////////

use piet_gpu_hal::{BindType, BufferUsage, ComputePassDescriptor, ShaderCode};

pub struct Render {
    config: Config,

    cfg_buf: piet_gpu_hal::Buffer,
    tile_buf: piet_gpu_hal::Buffer,

    // Working memory
    choice_buf: piet_gpu_hal::Buffer,
    out_buf: piet_gpu_hal::Buffer,

    //interval: piet_gpu_hal::Pipeline,
    pixels: piet_gpu_hal::Pipeline,
}

/// The configuration block passed to compute
///
/// Note: this should be kept in sync with the version in `METAL_PRELUDE`
/// above
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, Zeroable, Pod)]
pub struct RenderConfig {
    /// Total image size, in pixels.  This will be a multiple of `tile_size`.
    pub image_size: u32,

    /// Size of a render tile, in pixels
    pub tile_size: u32,

    /// Index of the X variable in `vars`, or `u32::MAX` if not present
    pub var_index_x: u32,

    /// Index of the Y variable in `vars`, or `u32::MAX` if not present
    pub var_index_y: u32,

    /// Index of the Z variable in `vars`, or `u32::MAX` if not present
    pub var_index_z: u32,
}

impl Render {
    pub fn new(prog: &Program, session: &piet_gpu_hal::Session) -> Self {
        let cfg_buf = session
            .create_buffer(
                std::mem::size_of::<RenderConfig>().try_into().unwrap(),
                BufferUsage::MAP_WRITE | BufferUsage::STORAGE,
            )
            .unwrap();
        let tile_buf = session
            .create_buffer(8, BufferUsage::MAP_WRITE | BufferUsage::STORAGE)
            .unwrap();
        let out_buf = session
            .create_buffer(8, BufferUsage::STORAGE | BufferUsage::MAP_READ)
            .unwrap();
        let choice_buf =
            session.create_buffer(8, BufferUsage::STORAGE).unwrap();

        let shader_f = prog.to_metal(Mode::Pixel);
        //let shader_i = prog.to_metal(Mode::Interval);

        // SAFETY: it's doing GPU stuff, so who knows?
        let pixels = unsafe {
            let pixels = session
                .create_compute_pipeline(
                    ShaderCode::Msl(&shader_f),
                    &[
                        BindType::BufReadOnly, // config
                        BindType::BufReadOnly, // tiles
                        BindType::BufReadOnly, // choices
                        BindType::Buffer,      // out
                    ],
                )
                .unwrap();
            /* TODO
            let interval = session
                .create_compute_pipeline(
                    ShaderCode::Msl(&shader_i),
                    &[
                        BindType::BufReadOnly,
                        BindType::Buffer, // choices
                        BindType::Buffer, // out
                    ],
                )
                .unwrap();
            (pixels, interval)
            */
            pixels
        };

        Self {
            config: prog.config().clone(),
            choice_buf,
            tile_buf,
            cfg_buf,
            out_buf,
            //interval,
            pixels,
        }
    }

    /// Sends the given data to a buffer, resizing to fit if needed
    unsafe fn send_to_buf<T: Pod>(
        session: &piet_gpu_hal::Session,
        buf: &mut piet_gpu_hal::Buffer,
        data: &[T],
    ) {
        if data.len() * std::mem::size_of::<T>()
            > buf.size().try_into().unwrap()
        {
            *buf = session
                .create_buffer_init(
                    data,
                    BufferUsage::MAP_WRITE | BufferUsage::STORAGE,
                )
                .unwrap();
        } else {
            buf.write(data).unwrap();
        }
    }

    unsafe fn resize_to_fit<T: Pod>(
        session: &piet_gpu_hal::Session,
        buf: &mut piet_gpu_hal::Buffer,
        count: usize,
    ) {
        let size_bytes = count * std::mem::size_of::<T>();
        if size_bytes > buf.size().try_into().unwrap() {
            *buf = session
                .create_buffer(
                    size_bytes.try_into().unwrap(),
                    BufferUsage::STORAGE | BufferUsage::MAP_READ,
                )
                .unwrap();
        }
    }

    /// # Safety
    /// It's doing GPU stuff, who knows?
    pub unsafe fn render(
        &mut self,
        size: usize,
        session: &piet_gpu_hal::Session,
    ) -> Vec<[u8; 4]> {
        let cfg = RenderConfig {
            tile_size: 8,
            image_size: size.try_into().unwrap(),
            var_index_x: usize::from(self.config.vars["X"])
                .try_into()
                .unwrap_or(u32::MAX),
            var_index_y: usize::from(self.config.vars["Y"])
                .try_into()
                .unwrap_or(u32::MAX),
            var_index_z: u32::MAX,
        };

        self.cfg_buf.write(std::slice::from_ref(&cfg)).unwrap();

        assert_eq!(
            size % (cfg.tile_size as usize),
            0,
            "Size must be a multiple of tile size"
        );
        let group_count = (size / cfg.tile_size as usize).pow(2);

        // Initialize tiles to contain every tile in the image
        let mut tiles: Vec<u32> = vec![];
        for x in 0..(cfg.image_size / cfg.tile_size) {
            let x = u16::try_from(x).unwrap();
            for y in 0..(cfg.image_size / cfg.tile_size) {
                let y = u16::try_from(y).unwrap();
                tiles.push((u32::from(x) << 16) | u32::from(y));
            }
        }
        Self::send_to_buf(session, &mut self.tile_buf, &tiles);

        // Initialize choices array. Each choice array is shared by a tile's
        // worth of threads in the thread group.
        let choices = std::iter::repeat(0b11)
            .take(group_count * self.config.choice_count)
            .collect::<Vec<u8>>();
        Self::send_to_buf(session, &mut self.choice_buf, &choices);

        // Resize out buffer to fit one `uchar4` per thread
        Self::resize_to_fit::<[u8; 4]>(session, &mut self.out_buf, size.pow(2));

        let descriptor_set = session
            .create_simple_descriptor_set(
                &self.pixels,
                &[
                    &self.cfg_buf,
                    &self.tile_buf,
                    &self.choice_buf,
                    &self.out_buf,
                ],
            )
            .unwrap();

        let query_pool = session.create_query_pool(2).unwrap();

        let mut cmd_buf = session.cmd_buf().unwrap();
        cmd_buf.begin();
        cmd_buf.reset_query_pool(&query_pool);
        {
            let mut pass = cmd_buf.begin_compute_pass(
                &ComputePassDescriptor::timer(&query_pool, 0, 1),
            );
            pass.dispatch(
                &self.pixels,
                &descriptor_set,
                (u32::try_from(group_count).unwrap(), 1, 1),
                (cfg.tile_size.pow(2), 1, 1),
            );
            pass.end();
        }

        cmd_buf.finish_timestamps(&query_pool);
        cmd_buf.host_barrier();
        cmd_buf.finish();

        let submitted = session.run_cmd_buf(cmd_buf, &[], &[]).unwrap();
        submitted.wait().unwrap();
        let timestamps = session.fetch_query_pool(&query_pool);

        let mut dst: Vec<[u8; 4]> = vec![];
        self.out_buf.read(&mut dst).unwrap();
        println!("{:?}", timestamps);
        println!("dst size: {}", self.out_buf.size());
        println!("dst len : {}", dst.len());

        println!("{:?}\ngroup cnt {}", cfg, group_count);

        dst
    }
}
