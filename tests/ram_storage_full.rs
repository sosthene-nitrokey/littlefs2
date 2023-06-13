use littlefs2::{
    const_ram_storage, driver::Storage as LfsStorage, fs::*, io::Result as LfsResult, path,
};

const BLOCK_SIZE: usize = 512;
const SIZE: usize = 8192;

const_ram_storage!(
    name=VolatileStorage,
    trait=LfsStorage,
    erase_value=0xff,
    read_size=16,
    write_size=BLOCK_SIZE,
    cache_size_ty=littlefs2::consts::U128,
    block_size=BLOCK_SIZE,
    block_count=SIZE/BLOCK_SIZE,
    lookahead_size_ty=littlefs2::consts::U1,
    filename_max_plus_one_ty=littlefs2::consts::U256,
    path_max_plus_one_ty=littlefs2::consts::U256,
    result=LfsResult,
);

#[test]
fn test() {
    let mut ram_storage = VolatileStorage::new();
    Filesystem::format(&mut ram_storage).unwrap();
    let res = Filesystem::mount_and_then(&mut ram_storage, |fs| {
        fs.create_dir(path!("/opcard"))?;
        fs.create_dir(path!("/opcard/sec"))?;
        fs.create_dir(path!("/fido"))?;
        fs.create_dir(path!("/fido/sec"))?;
        fs.create_file_and_then(
            path!("/opcard/sec/d80635e987f6828d6309ee74430440d2"),
            |file| file.write(&[0x42; 2378]),
        )?;

        fs.open_file_and_then(
            path!("/opcard/sec/d80635e987f6828d6309ee74430440d2"),
            |file| {
                let mut buf = [0; 2378];
                file.read(&mut buf)?;
                assert_eq!(buf, [0x42; 2378]);
                Ok(())
            },
        )?;
        fs.create_dir(path!("/fido/pub"))?;
        fs.create_file_and_then(path!("fido/sec/86db44e50171fb3689a9d67064e81834"), |file| {
            file.write(&[0x67; 36])
        })?;
        fs.create_file_and_then(path!("fido/sec/26ca1024e1888c083e1f30d04ecb2dbc"), |file| {
            file.write(&[0x67; 37])
        })?;
        Ok(())
    });
    std::fs::write("volatile.bin", &ram_storage.buf).unwrap();
    res.unwrap();
}
