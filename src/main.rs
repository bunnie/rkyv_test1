use core::mem::MaybeUninit;

use rkyv::{
    access_unchecked, api::{high::to_bytes_in, low::{to_bytes_in_with_alloc, LowSerializer}}, rancor::{Error, Failure, Panic, Strategy}, ser::{allocator::SubAllocator,
        writer::Buffer, Writer
    }, util::Align, with::{ArchiveWith, AsBox, InlineAsBox, SerializeWith}, Archive, Archived, Place, Resolver, Serialize, Portable
};

use std::marker::PhantomData;

use xous::{MemoryAddress, MemoryMessage, MemoryRange, Error as XousError};

#[derive(Archive, Serialize)]
enum Event {
    Spawn,
    Speak(String),
    Die,
}


use rkyv::{deserialize, Deserialize};

#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(
    // This will generate a PartialEq impl between our unarchived
    // and archived types
    compare(PartialEq),
    // Derives can be passed through to the generated type:
    derive(Debug),
)]
struct Test {
    int: u8,
    string: String,
    option: Option<Vec<i32>>,
    string2: String,
}


// this is for alloc test
#[derive(Archive, Serialize)]
struct Example<'a> {
    #[rkyv(with = InlineAsBox)]
    inner: &'a i32,
}

const PAGE_SIZE: usize = 4096;
const PAGE_POOL_SIZE: usize = 8;
#[repr(C, align(4096))]
pub struct Pool {
    pub bytes: [u8; PAGE_SIZE * PAGE_POOL_SIZE]
}

#[derive(Debug)]
pub struct IpcBuffer<'buf> {
    pages: MemoryRange,
    alloc_at: usize,
    slice: &'buf mut [u8],
    should_drop: bool,
    memory_message: Option<&'buf mut MemoryMessage>,
}
// what is a safe number for this? empirically, 20 bytes seems to be the overhead we've measured
const RKYV_OVERHEAD: usize = 32;

// fake some allocateable memory, simply bump-allocated
static mut PAGE_POOL: Pool = Pool {bytes: [0u8; PAGE_SIZE * PAGE_POOL_SIZE]};
static mut PAGE_PTR: usize = 0;

pub fn map_memory(len: usize) -> MemoryRange {
    assert!(len % 4096 == 0);
    let address = unsafe{& PAGE_POOL.bytes[PAGE_PTR..PAGE_PTR + len]};

    let mr = unsafe{MemoryRange::new(address.as_ptr() as usize, len).unwrap()};
    unsafe{ PAGE_PTR += len };
    mr
}

type Serializer<'a, 'b> =
    LowSerializer<'a, Buffer<'b>, SubAllocator<'a>, Panic>;

impl<'buf> IpcBuffer<'buf> {
    pub fn new(len: usize) -> Self {
        let len_to_page = (len + (PAGE_SIZE -1)) & !(PAGE_SIZE - 1);
        let alloc_start = len + RKYV_OVERHEAD;

        // Allocate enough memory to hold the requested data
        let new_mem = map_memory(len_to_page);

        IpcBuffer {
            pages: new_mem,
            slice: unsafe { core::slice::from_raw_parts_mut(new_mem.as_mut_ptr(), len_to_page) },
            alloc_at: alloc_start,
            should_drop: true,
            memory_message: None,
        }
    }

    pub fn into_buf<F, T>(src: &T) -> Self
    where
        F: for<'a, 'b> SerializeWith<T, Serializer<'a, 'b>>,
    {
        struct Wrap<'a, F, T>(&'a T, PhantomData<F>);

        impl<F, T> Archive for Wrap<'_, F, T>
        where
            F: ArchiveWith<T>,
        {
            type Archived = <F as ArchiveWith<T>>::Archived;
            type Resolver = <F as ArchiveWith<T>>::Resolver;

            fn resolve(
                &self,
                resolver: Self::Resolver,
                out: Place<Self::Archived>,
            ) {
                F::resolve_with(self.0, resolver, out)
            }
        }

        impl<'a, 'b, F, T> Serialize<Serializer<'a, 'b>> for Wrap<'_, F, T>
        where
            F: SerializeWith<T, Serializer<'a, 'b>>,
        {
            fn serialize(
                &self,
                serializer: &mut Serializer<'a, 'b>,
            ) -> Result<Self::Resolver, Panic> {
                F::serialize_with(self.0, serializer)
            }
        }
        let xous_buf = Self::new(core::mem::size_of::<T>());
        let (buf, scratch) = xous_buf.slice.split_at_mut(xous_buf.alloc_at);

        let wrap = Wrap(src, PhantomData::<F>);
        let writer = Buffer::from(buf);
        let maybe_uninit_slice: &mut [MaybeUninit<u8>] = unsafe {
            std::slice::from_raw_parts_mut(scratch.as_ptr() as *mut MaybeUninit<u8>, scratch.len())
        };
        let alloc = SubAllocator::new(maybe_uninit_slice);

        let _ = rkyv::api::low::to_bytes_in_with_alloc::<_, _, Panic>(&wrap, writer, alloc);
        xous_buf
    }

    pub fn to_original<T, E>(&self) -> core::result::Result<T, E>
    where
        // T: Archive + rkyv::Deserialize<T, rkyv::rancor::Strategy<rkyv::de::Unpool, E>>,
        // E: std::fmt::Debug, <T as Archive>::Archived: Deserialize<T, Strategy<rkyv::de::Pool, E>>
        T: Archive + rkyv::Deserialize<T, rkyv::rancor::Strategy<rkyv::de::Pool, E>>,
        E: std::fmt::Debug, <T as Archive>::Archived: Deserialize<T, Strategy<rkyv::de::Pool, E>>
    {
        let archived = unsafe{rkyv::from_bytes_unchecked::<T, E>(&self.slice[..]).unwrap()};
        // let archived = unsafe{rkyv::access_unchecked::<T>(&self.slice[..])};
        Ok(rkyv::deserialize::<T,E>(&archived).unwrap())
    }
}


fn main() {
    let event = Event::Speak("Help me!".to_string());
    let mut bytes = Align([MaybeUninit::uninit(); 256]);
    let buffer = to_bytes_in::<_, Error>(&event, Buffer::from(&mut *bytes))
        .expect("failed to serialize event");
    let archived = unsafe { access_unchecked::<Archived<Event>>(&buffer) };
    if let Archived::<Event>::Speak(message) = archived {
        assert_eq!(message.as_str(), "Help me!");
    } else {
        panic!("archived event was of the wrong type");
    }

    let mut output = Align([MaybeUninit::<u8>::uninit(); core::mem::size_of::<Test>() + 20]);
    let mut alloc = [MaybeUninit::<u8>::uninit(); 256];


    // with an alloc
    let value = Test {
        int: 42,
        string: "hello world with more stuff in it".to_string(),
        option: Some(vec![1, 2, 3, 4]),
        string2: "more stuff".to_string(),
    };

    println!("Size of Test: {}", core::mem::size_of::<Test>());

    let bytes = to_bytes_in_with_alloc::<_, _, Failure>(
        &value,
        Buffer::from(&mut *output),
        SubAllocator::new(&mut alloc),
    )
    .unwrap();

    // Or you can use the unsafe API for maximum performance
    let archived =
        unsafe { rkyv::access_unchecked::<ArchivedTest>(&bytes[..]) };
    assert_eq!(archived, &value);

    // And you can always deserialize back to the original type
    let deserialized = deserialize::<Test, Error>(archived).unwrap();
    assert_eq!(deserialized, value);

    let buf = IpcBuffer::into_buf::<AsBox, Test>(&value);

    let de = buf.to_original::<Test, rkyv::rancor::Error>().unwrap();
}
