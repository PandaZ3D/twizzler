//! This mod implements [UserContext] and [KernelMemoryContext] for virtual memory systems.

use core::ptr::NonNull;

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use twizzler_abi::{
    device::CacheType,
    object::{ObjID, Protections, MAX_SIZE},
    upcall::{
        MemoryAccessKind, MemoryContextViolationInfo, ObjectMemoryError, ObjectMemoryFaultInfo,
        UpcallInfo,
    },
};

use super::{InsertError, KernelMemoryContext, ObjectContextInfo, UserContext};
use crate::{
    arch::{address::VirtAddr, context::ArchContext},
    idcounter::{Id, IdCounter, StableId},
    memory::{
        pagetables::{
            ContiguousProvider, Mapper, MappingCursor, MappingFlags, MappingSettings,
            PhysAddrProvider, Table, ZeroPageProvider,
        },
        PhysAddr,
    },
    mutex::Mutex,
    obj::{self, ObjectRef},
    spinlock::Spinlock,
    thread::current_thread_ref,
};

use crate::{
    obj::{pages::Page, PageNumber},
    thread::current_memory_context,
};

/// A type that implements [Context] for virtual memory systems.
pub struct VirtContext {
    arch: ArchContext,
    upcall: Spinlock<Option<VirtAddr>>,
    slots: Mutex<SlotMgr>,
    id: Id<'static>,
}

static CONTEXT_IDS: IdCounter = IdCounter::new();

#[derive(Default)]
struct SlotMgr {
    slots: BTreeMap<Slot, VirtContextSlot>,
    objs: BTreeMap<ObjID, Vec<Slot>>,
}

/// A representation of a slot number.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Ord, Eq)]
pub struct Slot(usize);

impl Slot {
    fn start_vaddr(&self) -> VirtAddr {
        VirtAddr::new((self.0 * MAX_SIZE) as u64).unwrap()
    }

    fn raw(&self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for Slot {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        let vaddr = VirtAddr::new((value * MAX_SIZE) as u64).map_err(|_| ())?;
        vaddr.try_into()
    }
}

impl TryFrom<VirtAddr> for Slot {
    type Error = ();

    fn try_from(value: VirtAddr) -> Result<Self, Self::Error> {
        if value.is_kernel() {
            Err(())
        } else {
            Ok(Self(value.raw() as usize / MAX_SIZE))
        }
    }
}

impl SlotMgr {
    fn get(&self, slot: &Slot) -> Option<&VirtContextSlot> {
        self.slots.get(slot)
    }

    fn insert(&mut self, slot: Slot, id: ObjID, info: VirtContextSlot) {
        self.slots.insert(slot, info);
        let list = self.objs.entry(id).or_default();
        list.push(slot);
    }

    fn remove(&mut self, slot: Slot) -> Option<VirtContextSlot> {
        if let Some(info) = self.slots.remove(&slot) {
            self.objs.remove(&info.obj.id());
            Some(info)
        } else {
            None
        }
    }

    fn obj_to_slots(&self, id: ObjID) -> Option<&[Slot]> {
        self.objs.get(&id).map(|x| x.as_slice())
    }
}

struct ObjectPageProvider<'a> {
    page: &'a Page,
}

impl<'a> PhysAddrProvider for ObjectPageProvider<'a> {
    fn peek(&mut self) -> (crate::arch::address::PhysAddr, usize) {
        (self.page.physical_address(), PageNumber::PAGE_SIZE)
    }

    fn consume(&mut self, _len: usize) {}
}

impl Default for VirtContext {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtContext {
    fn __new(arch: ArchContext) -> Self {
        Self {
            arch,
            upcall: Spinlock::new(None),
            slots: Mutex::new(SlotMgr::default()),
            id: CONTEXT_IDS.next(),
        }
    }

    /// Construct a new context for the kernel.
    pub fn new_kernel() -> Self {
        Self::__new(ArchContext::new_kernel())
    }

    /// Construct a new context for userspace.
    pub fn new() -> Self {
        Self::__new(ArchContext::new())
    }

    /// Init a context for being the kernel context, and clone the mappings from the bootstrap context.
    pub(super) fn init_kernel_context(&self) {
        let proto = unsafe { Mapper::current() };
        let rm = proto.readmap(MappingCursor::new(
            VirtAddr::start_kernel_memory(),
            usize::MAX,
        ));
        for map in rm.coalesce() {
            let cursor = MappingCursor::new(map.vaddr(), map.len());
            let mut phys = ContiguousProvider::new(map.paddr(), map.len());
            let settings = MappingSettings::new(
                map.settings().perms(),
                map.settings().cache(),
                map.settings().flags() | MappingFlags::GLOBAL,
            );
            self.arch.map(cursor, &mut phys, &settings);
        }

        // ID-map the lower memory. This is needed by some systems to boot secondary CPUs. This mapping is cleared by
        // the call to prep_smp later.
        let id_len = 0x100000000; // 4GB
        let cursor = MappingCursor::new(
            VirtAddr::new(Table::level_to_page_size(0).try_into().unwrap()).unwrap(),
            id_len,
        );
        let mut phys = ContiguousProvider::new(
            PhysAddr::new(Table::level_to_page_size(0).try_into().unwrap()).unwrap(),
            id_len,
        );
        let settings = MappingSettings::new(
            Protections::READ | Protections::WRITE | Protections::EXEC,
            CacheType::WriteBack,
            MappingFlags::empty(),
        );
        self.arch.map(cursor, &mut phys, &settings);
    }
}

impl UserContext for VirtContext {
    type UpcallInfo = VirtAddr;
    type MappingInfo = Slot;

    fn set_upcall(&self, target: Self::UpcallInfo) {
        *self.upcall.lock() = Some(target);
    }

    fn get_upcall(&self) -> Option<Self::UpcallInfo> {
        *self.upcall.lock()
    }

    fn switch_to(&self) {
        self.arch.switch_to();
    }

    fn insert_object(
        self: &Arc<Self>,
        slot: Slot,
        object_info: &ObjectContextInfo,
    ) -> Result<(), InsertError> {
        let new_slot_info = VirtContextSlot {
            obj: object_info.object().clone(),
            slot,
            prot: object_info.prot(),
            cache: object_info.cache(),
        };
        object_info.object().add_context(self);
        let mut slots = self.slots.lock();
        if let Some(info) = slots.get(&slot) {
            if info != &new_slot_info {
                return Err(InsertError::Occupied);
            }
            return Ok(());
        }
        slots.insert(slot, object_info.object().id(), new_slot_info);
        Ok(())
    }

    fn lookup_object(&self, info: Self::MappingInfo) -> Option<ObjectContextInfo> {
        let slots = self.slots.lock();
        slots.get(&info).map(|info| info.into())
    }

    fn invalidate_object(
        &self,
        obj: ObjID,
        range: &core::ops::Range<PageNumber>,
        mode: obj::InvalidateMode,
    ) {
        let start = range.start.as_byte_offset();
        let len = range.end.as_byte_offset() - start;
        let slots = self.slots.lock();
        if let Some(maps) = slots.obj_to_slots(obj) {
            for map in maps {
                let info = slots
                    .get(map)
                    .expect("invalid slot info for a mapped object");
                match mode {
                    obj::InvalidateMode::Full => {
                        self.arch.unmap(info.mapping_cursor(start, len));
                    }
                    obj::InvalidateMode::WriteProtect => {
                        self.arch.change(
                            info.mapping_cursor(start, len),
                            &info.mapping_settings(true),
                        );
                    }
                }
            }
        }
    }

    fn remove_object(&self, info: Self::MappingInfo) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.remove(info) {
            slot.obj.remove_context(self.id.value());
        }
    }
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct VirtContextSlot {
    obj: ObjectRef,
    slot: Slot,
    prot: Protections,
    cache: CacheType,
}

impl From<&VirtContextSlot> for ObjectContextInfo {
    fn from(info: &VirtContextSlot) -> Self {
        ObjectContextInfo::new(info.obj.clone(), info.prot, info.cache)
    }
}

impl VirtContextSlot {
    fn mapping_cursor(&self, start: usize, len: usize) -> MappingCursor {
        MappingCursor::new(self.slot.start_vaddr().offset(start).unwrap(), len)
    }

    fn mapping_settings(&self, wp: bool) -> MappingSettings {
        let mut prot = self.prot;
        if wp {
            prot.remove(Protections::WRITE);
        }
        MappingSettings::new(prot, self.cache, MappingFlags::USER)
    }

    fn phys_provider<'a>(&self, page: &'a Page) -> ObjectPageProvider<'a> {
        ObjectPageProvider { page }
    }
}

impl Drop for VirtContext {
    fn drop(&mut self) {
        let id = self.id().value();
        // cleanup and object's context info
        for info in self.slots.get_mut().slots.values() {
            info.obj.remove_context(id)
        }
    }
}

pub const HEAP_MAX_LEN: usize = 0x0000001000000000 / 16; //4GB

struct GlobalPageAlloc {
    alloc: linked_list_allocator::Heap,
    end: VirtAddr,
}

impl GlobalPageAlloc {
    fn extend(&mut self, len: usize, mapper: &VirtContext) {
        let cursor = MappingCursor::new(self.end, len);
        let mut phys = ZeroPageProvider::default();
        let settings = MappingSettings::new(
            Protections::READ | Protections::WRITE,
            CacheType::WriteBack,
            MappingFlags::GLOBAL,
        );
        mapper.arch.map(cursor, &mut phys, &settings);
        self.end = self.end.offset(len).unwrap();
        // Safety: the extension is backed by memory that is directly after the previous call to extend.
        unsafe {
            self.alloc.extend(len);
        }
    }

    fn init(&mut self, mapper: &VirtContext) {
        let len = 2 * 1024 * 1024;
        let cursor = MappingCursor::new(self.end, len);
        let mut phys = ZeroPageProvider::default();
        let settings = MappingSettings::new(
            Protections::READ | Protections::WRITE,
            CacheType::WriteBack,
            MappingFlags::GLOBAL,
        );
        mapper.arch.map(cursor, &mut phys, &settings);
        self.end = self.end.offset(len).unwrap();
        // Safety: the initial is backed by memory.
        unsafe {
            self.alloc.init(VirtAddr::HEAP_START.as_mut_ptr(), len);
        }
    }
}

// Safety: the internal heap contains raw pointers, which are not Send. However, the heap is globally mapped and static
// for the lifetime of the kernel.
unsafe impl Send for GlobalPageAlloc {}

static GLOBAL_PAGE_ALLOC: Spinlock<GlobalPageAlloc> = Spinlock::new(GlobalPageAlloc {
    alloc: linked_list_allocator::Heap::empty(),
    end: VirtAddr::HEAP_START,
});

impl KernelMemoryContext for VirtContext {
    fn allocate_chunk(&self, layout: core::alloc::Layout) -> NonNull<u8> {
        let mut glb = GLOBAL_PAGE_ALLOC.lock();
        let res = glb.alloc.allocate_first_fit(layout);
        match res {
            Err(_) => {
                let size = layout
                    .pad_to_align()
                    .size()
                    .next_multiple_of(crate::memory::pagetables::Table::level_to_page_size(0))
                    * 2;
                glb.extend(size, self);
                glb.alloc.allocate_first_fit(layout).unwrap()
            }
            Ok(x) => x,
        }
    }

    unsafe fn deallocate_chunk(&self, layout: core::alloc::Layout, ptr: NonNull<u8>) {
        let mut glb = GLOBAL_PAGE_ALLOC.lock();
        glb.alloc.deallocate(ptr, layout);
    }

    fn init_allocator(&self) {
        let mut glb = GLOBAL_PAGE_ALLOC.lock();
        glb.init(self);
    }

    fn prep_smp(&self) {
        self.arch.unmap(MappingCursor::new(
            VirtAddr::start_user_memory(),
            VirtAddr::end_user_memory() - VirtAddr::start_user_memory(),
        ));
    }
}

impl StableId for VirtContext {
    fn id(&self) -> &Id<'_> {
        &self.id
    }
}

bitflags::bitflags! {
    pub struct PageFaultFlags : u32 {
        const USER = 1;
        const INVALID = 2;
        const PRESENT = 4;
    }
}

pub fn page_fault(addr: VirtAddr, cause: MemoryAccessKind, flags: PageFaultFlags, ip: VirtAddr) {
    //logln!("page-fault: {:?} {:?} {:?} ip={:?}", addr, cause, flags, ip);
    if flags.contains(PageFaultFlags::INVALID) {
        panic!("page table contains invalid bits for address {:?}", addr);
    }
    if !flags.contains(PageFaultFlags::USER) && addr.is_kernel() {
        panic!(
            "kernel page-fault at IP {:?} caused by {:?} to/from {:?} with flags {:?}",
            ip, cause, addr, flags
        );
    } else {
        if addr.is_kernel() {
            current_thread_ref()
                .unwrap()
                .send_upcall(UpcallInfo::MemoryContextViolation(
                    MemoryContextViolationInfo::new(addr.raw(), cause),
                ));
            return;
        }

        let ctx = current_memory_context().unwrap_or_else(|| panic!("page fault in userland with no memory context at IP {:?} caused by {:?} to/from {:?} with flags {:?}", ip, cause, addr, flags));
        let slot = match addr.try_into() {
            Ok(s) => s,
            Err(_) => {
                current_thread_ref()
                    .unwrap()
                    .send_upcall(UpcallInfo::MemoryContextViolation(
                        MemoryContextViolationInfo::new(addr.raw(), cause),
                    ));
                return;
            }
        };

        let slot_mgr = ctx.slots.lock();

        if let Some(info) = slot_mgr.get(&slot) {
            let page_number = PageNumber::from_address(addr);
            let mut obj_page_tree = info.obj.lock_page_tree();
            if page_number.is_zero() {
                current_thread_ref()
                    .unwrap()
                    .send_upcall(UpcallInfo::ObjectMemoryFault(ObjectMemoryFaultInfo::new(
                        info.obj.id(),
                        ObjectMemoryError::NullPageAccess,
                        cause,
                    )));
                return;
            }
            if page_number.as_byte_offset() >= MAX_SIZE {
                current_thread_ref()
                    .unwrap()
                    .send_upcall(UpcallInfo::ObjectMemoryFault(ObjectMemoryFaultInfo::new(
                        info.obj.id(),
                        ObjectMemoryError::OutOfBounds(page_number.as_byte_offset()),
                        cause,
                    )));
                return;
            }

            if let Some((page, cow)) =
                obj_page_tree.get_page(page_number, cause == MemoryAccessKind::Write)
            {
                ctx.arch.map(
                    info.mapping_cursor(page_number.as_byte_offset(), PageNumber::PAGE_SIZE),
                    &mut info.phys_provider(&page),
                    &info.mapping_settings(cow),
                );
            } else {
                let page = Page::new();
                obj_page_tree.add_page(page_number, page);
                let (page, cow) = obj_page_tree
                    .get_page(page_number, cause == MemoryAccessKind::Write)
                    .unwrap();
                ctx.arch.map(
                    info.mapping_cursor(page_number.as_byte_offset(), PageNumber::PAGE_SIZE),
                    &mut info.phys_provider(&page),
                    &info.mapping_settings(cow),
                );
            }
        } else {
            current_thread_ref()
                .unwrap()
                .send_upcall(UpcallInfo::MemoryContextViolation(
                    MemoryContextViolationInfo::new(addr.raw(), cause),
                ));
        }
    }
}