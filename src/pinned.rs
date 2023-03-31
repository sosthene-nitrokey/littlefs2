use core::pin::pin;
use core::slice;
use core::{marker::PhantomPinned, mem::MaybeUninit, pin::Pin};

use bitflags::bitflags;
use generic_array::typenum::Unsigned;
use pin_project::{pin_project, pinned_drop};

use crate::{
    driver,
    io::{self, Result},
    ll,
    path::{Path, PathBuf},
};

pub trait AnyStorage: Unpin {
    type RealStorage: driver::Storage;
    fn storage_mut(&mut self) -> &mut Self::RealStorage;
}

// so far, don't need `heapless-bytes`.
pub type Bytes<SIZE> = generic_array::GenericArray<u8, SIZE>;

#[pin_project]
struct Cache<Storage: driver::Storage> {
    #[pin]
    read: Bytes<Storage::CACHE_SIZE>,
    #[pin]
    write: Bytes<Storage::CACHE_SIZE>,
    // lookahead: aligned::Aligned<aligned::A4, Bytes<Storage::LOOKAHEAD_SIZE>>,
    #[pin]
    lookahead: generic_array::GenericArray<u64, Storage::LOOKAHEAD_SIZE>,
    #[pin]
    __: PhantomPinned,
}

impl<S: driver::Storage> Cache<S> {
    pub fn new() -> Self {
        Self {
            read: Default::default(),
            write: Default::default(),
            lookahead: Default::default(),
            __: PhantomPinned,
        }
    }
}

impl<S: driver::Storage> Default for Cache<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[pin_project]
struct Allocation<Storage: driver::Storage> {
    #[pin]
    cache: Cache<Storage>,
    config: ll::lfs_config,
    #[pin]
    state: ll::lfs_t,
    __: PhantomPinned,
}

impl<Storage: driver::Storage> Allocation<Storage> {
    pub fn new() -> Allocation<Storage> {
        let read_size: u32 = Storage::READ_SIZE as _;
        let write_size: u32 = Storage::WRITE_SIZE as _;
        let block_size: u32 = Storage::BLOCK_SIZE as _;
        let cache_size: u32 = <Storage as driver::Storage>::CACHE_SIZE::U32;
        let lookahead_size: u32 = 8 * <Storage as driver::Storage>::LOOKAHEAD_SIZE::U32;
        let block_cycles: i32 = Storage::BLOCK_CYCLES as _;
        let block_count: u32 = Storage::BLOCK_COUNT as _;

        debug_assert!(block_cycles >= -1);
        debug_assert!(block_cycles != 0);
        debug_assert!(block_count > 0);

        debug_assert!(read_size > 0);
        debug_assert!(write_size > 0);
        // https://github.com/ARMmbed/littlefs/issues/264
        // Technically, 104 is enough.
        debug_assert!(block_size >= 128);
        debug_assert!(cache_size > 0);
        debug_assert!(lookahead_size > 0);

        // cache must be multiple of read
        debug_assert!(read_size <= cache_size);
        debug_assert!(cache_size % read_size == 0);

        // cache must be multiple of write
        debug_assert!(write_size <= cache_size);
        debug_assert!(cache_size % write_size == 0);

        // block must be multiple of cache
        debug_assert!(cache_size <= block_size);
        debug_assert!(block_size % cache_size == 0);

        let cache = Cache::new();

        let filename_max_plus_one: u32 = crate::consts::FILENAME_MAX_PLUS_ONE;
        debug_assert!(filename_max_plus_one > 1);
        debug_assert!(filename_max_plus_one <= 1_022 + 1);
        // limitation of ll-bindings
        debug_assert!(filename_max_plus_one == 255 + 1);
        let path_max_plus_one: u32 = crate::consts::PATH_MAX_PLUS_ONE as _;
        // TODO: any upper limit?
        debug_assert!(path_max_plus_one >= filename_max_plus_one);
        let file_max = crate::consts::FILEBYTES_MAX;
        assert!(file_max > 0);
        assert!(file_max <= 2_147_483_647);
        // limitation of ll-bindings
        assert!(file_max == 2_147_483_647);
        let attr_max: u32 = crate::consts::ATTRBYTES_MAX;
        assert!(attr_max > 0);
        assert!(attr_max <= 1_022);
        // limitation of ll-bindings
        assert!(attr_max == 1_022);

        let config = ll::lfs_config {
            context: core::ptr::null_mut(),
            read: Some(<Filesystem<Storage>>::lfs_config_read),
            prog: Some(<Filesystem<Storage>>::lfs_config_prog),
            erase: Some(<Filesystem<Storage>>::lfs_config_erase),
            sync: Some(<Filesystem<Storage>>::lfs_config_sync),
            // read: None,
            // prog: None,
            // erase: None,
            // sync: None,
            read_size,
            prog_size: write_size,
            block_size,
            block_count,
            block_cycles,
            cache_size,
            lookahead_size,

            read_buffer: core::ptr::null_mut(),
            prog_buffer: core::ptr::null_mut(),
            lookahead_buffer: core::ptr::null_mut(),

            name_max: filename_max_plus_one.wrapping_sub(1),
            file_max,
            attr_max,
        };

        Self {
            cache,
            state: unsafe { MaybeUninit::zeroed().assume_init() },
            config,
            __: PhantomPinned,
        }
    }
}

bitflags! {
    /// Definition of file open flags which can be mixed and matched as appropriate. These definitions
    /// are reminiscent of the ones defined by POSIX.
    struct FileOpenFlags: u32 {
        /// Open file in read only mode.
        const READ = 0x1;
        /// Open file in write only mode.
        const WRITE = 0x2;
        /// Open file for reading and writing.
        const READWRITE = Self::READ.bits | Self::WRITE.bits;
        /// Create the file if it does not exist.
        const CREATE = 0x0100;
        /// Fail if creating a file that already exists.
        /// TODO: Good name for this
        const EXCL = 0x0200;
        /// Truncate the file if it already exists.
        const TRUNCATE = 0x0400;
        /// Open the file in append only mode.
        const APPEND = 0x0800;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenOptions(FileOpenFlags);

/// The state of a `File`. Pre-allocate with `File::allocate`.
#[pin_project(PinnedDrop)]
pub struct RawFile<S: driver::Storage> {
    #[pin]
    cache: Bytes<S::CACHE_SIZE>,
    #[pin]
    state: ll::lfs_file_t,
    __: PhantomPinned,
    config: ll::lfs_file_config,
}
#[pinned_drop]
impl<S: driver::Storage> PinnedDrop for RawFile<S> {
    fn drop(self: Pin<&mut Self>) {
        self.close(todo!("How to get the storage here?")).ok();
    }
}

impl<S: driver::Storage> RawFile<S> {
    /// Safety: The caller must ensure that it is initialized befor being dropped
    pub unsafe fn new_uninit() -> Self {
        let cache_size: u32 = <S as driver::Storage>::CACHE_SIZE::to_u32();
        debug_assert!(cache_size > 0);
        unsafe { MaybeUninit::zeroed().assume_init() }
    }
    fn close(self: Pin<&mut Self>, fs: Pin<&mut Filesystem<S>>) -> Result<()> {
        let this = self.project();
        let fs = fs.project();
        let alloc = fs.allocation.project();

        let return_code = unsafe {
            ll::lfs_file_close(
                alloc.state.get_unchecked_mut(),
                this.state.get_unchecked_mut(),
            )
        };
        io::result_from((), return_code)
    }
}

#[pin_project]
pub struct Filesystem<Storage: driver::Storage> {
    #[pin]
    allocation: Allocation<Storage>,
    storage: Storage,
    initialized: bool,
}

impl<Storage: driver::Storage> Filesystem<Storage> {
    pub fn mount(storage: Storage) -> Self {
        Self {
            allocation: Allocation::new(),
            storage,
            initialized: false,
        }
    }

    fn configure(self: Pin<&mut Self>) {
        let this = self.project();
        let alloc = this.allocation.project();
        let config = alloc.config;
        let mut cache = alloc.cache.project();
        config.context = this.storage as *mut _ as *mut cty::c_void;

        config.read_buffer = &mut cache.read as *mut _ as *mut cty::c_void;
        config.prog_buffer = &mut cache.write as *mut _ as *mut cty::c_void;
        config.lookahead_buffer = &mut cache.lookahead as *mut _ as *mut cty::c_void;
    }
    fn ensure_initialized(mut self: Pin<&mut Self>) -> Result<()> {
        self.as_mut().configure();
        let this = self.project();
        if *this.initialized {
            return Ok(());
        }
        let alloc = this.allocation.project();

        let ret = unsafe { ll::lfs_mount(alloc.state.get_unchecked_mut(), alloc.config) };
        io::result_from((), ret)
    }

    pub fn format_storage(storage: &mut Storage) -> Result<()> {
        let this = pin!(Filesystem::mount(storage));
        this.format()
    }

    fn format(mut self: Pin<&mut Self>) -> Result<()> {
        self.as_mut().configure();
        let this = self.project();
        let alloc = this.allocation.project();
        let ret = unsafe { ll::lfs_format(alloc.state.get_unchecked_mut(), alloc.config) };
        io::result_from((), ret)
    }

    /// Creates a new, empty directory at the provided path.
    pub fn create_dir(mut self: Pin<&mut Self>, path: &Path) -> Result<()> {
        self.as_mut().ensure_initialized()?;
        #[cfg(test)]
        println!("creating {:?}", path);
        let this = self.project();
        let alloc = this.allocation.project();
        let return_code = unsafe { ll::lfs_mkdir(alloc.state.get_unchecked_mut(), path.as_ptr()) };
        io::result_from((), return_code)
    }
    pub fn create_dir_all(mut self: Pin<&mut Self>, path: &Path) -> Result<()> {
        self.as_mut().ensure_initialized()?;
        // Placeholder implementation!
        // - Path should gain a few methods
        // - Maybe should pull in `heapless-bytes` (and merge upstream into `heapless`)
        // - All kinds of sanity checks and possible logic errors possible...

        let path_slice = path.as_ref().as_bytes();
        for i in 0..path_slice.len() {
            if path_slice[i] == b'/' {
                let dir = PathBuf::from(&path_slice[..i]);
                #[cfg(test)]
                println!("generated PathBuf dir {:?} using i = {}", &dir, i);
                match self.as_mut().create_dir(&dir) {
                    Ok(_) => {}
                    Err(io::Error::EntryAlreadyExisted) => {}
                    error => {
                        panic!("{:?}", &error);
                    }
                }
            }
        }
        match self.create_dir(path) {
            Ok(_) => {}
            Err(io::Error::EntryAlreadyExisted) => {}
            error => {
                panic!("{:?}", &error);
            }
        }
        Ok(())
    }

    pub fn open_file(
        mut self: Pin<&mut Self>,
        path: &Path,
        file: Pin<&mut RawFile<Storage>>,
        options: OpenOptions,
    ) -> Result<()> {
        self.as_mut().ensure_initialized()?;

        let this = self.project();
        let alloc = this.allocation.project();
        let file = file.project();
        file.config.buffer =
            unsafe { file.cache.get_unchecked_mut() as *mut _ as *mut cty::c_void };
        let return_code = unsafe {
            ll::lfs_file_opencfg(
                alloc.state.get_unchecked_mut(),
                file.state.get_unchecked_mut(),
                path.as_ptr(),
                options.0.bits() as i32,
                file.config,
            )
        };
        io::result_from((), return_code)
    }

    pub fn open_file_and_then<R>(
        self: Pin<&mut Self>,
        path: &Path,
        file: Pin<&mut RawFile<Storage>>,
        options: OpenOptions,
        f: impl FnOnce(Pin<&mut RawFile<Storage>>) -> Result<R>,
    ) -> Result<R> {
        let file_alloc = pin!(unsafe { RawFile::new_uninit() });
        self.open_file(path, file, options)?;
        f(file_alloc)
    }
}

impl<Storage: driver::Storage> Filesystem<Storage> {
    /// C callback interface used by LittleFS to read data with the lower level system below the
    /// filesystem.
    extern "C" fn lfs_config_read(
        c: *const ll::lfs_config,
        block: ll::lfs_block_t,
        off: ll::lfs_off_t,
        buffer: *mut cty::c_void,
        size: ll::lfs_size_t,
    ) -> cty::c_int {
        // println!("in lfs_config_read for {} bytes", size);
        let storage = unsafe { &mut *((*c).context as *mut Storage) };
        debug_assert!(!c.is_null());
        let block_size = unsafe { c.read().block_size };
        let off = (block * block_size + off) as usize;
        let buf: &mut [u8] = unsafe { slice::from_raw_parts_mut(buffer as *mut u8, size as usize) };

        io::error_code_from(storage.read(off, buf))
    }

    /// C callback interface used by LittleFS to program data with the lower level system below the
    /// filesystem.
    extern "C" fn lfs_config_prog(
        c: *const ll::lfs_config,
        block: ll::lfs_block_t,
        off: ll::lfs_off_t,
        buffer: *const cty::c_void,
        size: ll::lfs_size_t,
    ) -> cty::c_int {
        // println!("in lfs_config_prog");
        let storage = unsafe { &mut *((*c).context as *mut Storage) };
        debug_assert!(!c.is_null());
        // let block_size = unsafe { c.read().block_size };
        let block_size = Storage::BLOCK_SIZE as u32;
        let off = (block * block_size + off) as usize;
        let buf: &[u8] = unsafe { slice::from_raw_parts(buffer as *const u8, size as usize) };

        io::error_code_from(storage.write(off, buf))
    }

    /// C callback interface used by LittleFS to erase data with the lower level system below the
    /// filesystem.
    extern "C" fn lfs_config_erase(c: *const ll::lfs_config, block: ll::lfs_block_t) -> cty::c_int {
        // println!("in lfs_config_erase");
        let storage = unsafe { &mut *((*c).context as *mut Storage) };
        let off = block as usize * Storage::BLOCK_SIZE as usize;

        io::error_code_from(storage.erase(off, Storage::BLOCK_SIZE as usize))
    }

    /// C callback interface used by LittleFS to sync data with the lower level interface below the
    /// filesystem. Note that this function currently does nothing.
    extern "C" fn lfs_config_sync(_c: *const ll::lfs_config) -> i32 {
        // println!("in lfs_config_sync");
        // Do nothing; we presume that data is synchronized.
        0
    }
}
