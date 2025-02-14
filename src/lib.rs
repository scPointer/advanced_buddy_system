#![cfg_attr(feature = "const_fn", feature(const_mut_refs, const_fn_fn_ptr_basics))]
//#![no_std]

#[cfg(test)]
#[macro_use]
extern crate std;

#[cfg(feature = "use_spin")]
extern crate spin;

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cmp::{max, min};
use core::fmt;
use core::mem::size_of;
#[cfg(feature = "use_spin")]
use core::ops::Deref;
use core::ptr::NonNull;
use rand_chacha::rand_core::block;
#[cfg(feature = "use_spin")]
use spin::Mutex;

mod frame;
pub mod linked_list;
#[cfg(test)]
mod test;

pub use frame::*;

/// A heap that uses buddy system with configurable order.
///
/// # Usage
///
/// Create a heap and add a memory region to it:
/// ```
/// use buddy_system_allocator::*;
/// # use core::mem::size_of;
/// let mut heap = Heap::<32>::empty();
/// # let space: [usize; 100] = [0; 100];
/// # let begin: usize = space.as_ptr() as usize;
/// # let end: usize = begin + 100 * size_of::<usize>();
/// # let size: usize = 100 * size_of::<usize>();
/// unsafe {
///     heap.init(begin, size);
///     // or
///     heap.add_to_heap(begin, end);
/// }
/// ```
pub struct Heap<const ORDER: usize> {
    // buddy system with max order of `ORDER`
    free_list: [linked_list::LinkedList; ORDER],

    // statistics
    user: usize,
    allocated: usize,
    total: usize,
}

impl<const ORDER: usize> Heap<ORDER> {
    /// Create an empty heap
    pub const fn new() -> Self {
        Heap {
            free_list: [linked_list::LinkedList::new(); ORDER],
            user: 0,
            allocated: 0,
            total: 0,
        }
    }

    /// Create an empty heap
    pub const fn empty() -> Self {
        Self::new()
    }

    /// Add a range of memory [start, end) to the heap
    pub unsafe fn add_to_heap(&mut self, mut start: usize, mut end: usize) {
        // avoid unaligned access on some platforms
        start = (start + size_of::<usize>() - 1) & (!size_of::<usize>() + 1);
        end = end & (!size_of::<usize>() + 1);
        assert!(start <= end);

        let mut total = 0;
        let mut current_start = start;

        while current_start + size_of::<usize>() <= end {
            let lowbit = current_start & (!current_start + 1);
            let size = min(lowbit, prev_power_of_two(end - current_start));
            total += size;
            println!(
                "Init ({}){}, at {:x}",
                size.trailing_zeros(),
                size,
                current_start
            );
            self.free_list[size.trailing_zeros() as usize].push(current_start as *mut usize);
            current_start += size;
        }

        self.total += total;
    }

    /// Add a range of memory [start, start+size) to the heap
    pub unsafe fn init(&mut self, start: usize, size: usize) {
        self.add_to_heap(start, start + size);
    }

    /// Alloc a range of memory from the heap satifying `layout` requirements
    pub fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // align 问题:
        // 原来的实现中，分配出去的块的 align 至少是 layout.size()，所以不需要考虑 unaligned 问题
        // 现在的实现中，这一点仍然保持
        let real_size = layout.size().max(layout.align()).max(size_of::<usize>());
        let block_size = real_size.max(layout.size().next_power_of_two());
        println!("Req {} / {}", real_size, block_size);
        let class = block_size.trailing_zeros() as usize;
        for i in class..self.free_list.len() {
            // Find the first non-empty size class
            if !self.free_list[i].is_empty() {
                // Split buffers
                for j in (class + 1..i + 1).rev() {
                    let block = self.free_list[j].pop()?;
                    println!("Sep ({}){}", j, 1 << j);
                    unsafe {
                        self.free_list[j - 1].push((block as usize + (1 << (j - 1))) as *mut usize);
                        self.free_list[j - 1].push(block);
                    }
                }
                let result = NonNull::new(
                    self.free_list[class]
                        .pop()
                        .expect("current block should have free space now")
                        as *mut u8,
                )?;
                println!(
                    "Alc ({}){}, at {:x}",
                    class,
                    1 << class,
                    result.as_ptr() as usize
                );
                // 需要还回去的内存区间是 [left_start, block_end)
                let block_end = result.as_ptr() as usize + block_size;
                let mut left_start = result.as_ptr() as usize + real_size;
                while left_start < block_end {
                    let lowbit = left_start & (!left_start + 1);
                    let class = lowbit.trailing_zeros() as usize;
                    println!("Ret ({}){}, at {:x}", class, lowbit, left_start);
                    unsafe {
                        self.free_list[class].push(left_start as *mut usize);
                    }
                    left_start += lowbit;
                }
                self.user += layout.size();
                self.allocated += real_size;
                return Some(result);
            }
        }
        None
    }

    unsafe fn push_and_try_merge(&mut self, addr: usize, class: usize) {
        // Put back into free list
        self.free_list[class].push(addr as *mut usize);
        // Merge free buddy lists
        let mut current_ptr = addr;
        let mut current_class = class;
        while current_class < self.free_list.len() {
            let buddy = current_ptr ^ (1 << current_class);
            let mut flag = false;
            for block in self.free_list[current_class].iter_mut() {
                if block.value() as usize == buddy {
                    block.pop();
                    flag = true;
                    break;
                }
            }

            // Free buddy found
            if flag {
                self.free_list[current_class].pop();
                current_ptr = min(current_ptr, buddy);
                current_class += 1;
                println!("MergeTo ({}){}, at {:x}", current_class, 1 << current_class, current_ptr);
                self.free_list[current_class].push(current_ptr as *mut usize);
            } else {
                break;
            }
        }
    }
    /// Dealloc a range of memory from the heap
    pub fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let real_size = layout.size().max(layout.align()).max(size_of::<usize>());
        let block_size = real_size.max(layout.size().next_power_of_two());
        println!("DealcReq {} / {}, at {:x}", real_size, block_size, ptr.as_ptr() as usize);
        // 如果区间没有被拆分，则只 dealloc 一次
        if real_size == block_size {
            unsafe {
                self.push_and_try_merge(ptr.as_ptr() as usize, block_size.trailing_zeros() as usize);
            }
            return;
        }
        // 需要拆分的内存区间是 [left_start, real_end)
        let mut real_end = ptr.as_ptr() as usize + real_size;
        let left_start = ptr.as_ptr() as usize;
        while left_start < real_end {
            let lowbit = real_end & (!real_end + 1);
            let class = lowbit.trailing_zeros() as usize;
            println!("Dealc ({}){}, at {:x}", class, lowbit, real_end - lowbit);
            unsafe {
                self.push_and_try_merge(real_end - lowbit, class);
            }
            real_end -= lowbit;
        }

        self.user -= layout.size();
        self.allocated -= real_size;
    }

    /// Return the number of bytes that user requests
    pub fn stats_alloc_user(&self) -> usize {
        self.user
    }

    /// Return the number of bytes that are actually allocated
    pub fn stats_alloc_actual(&self) -> usize {
        self.allocated
    }

    /// Return the total number of bytes in the heap
    pub fn stats_total_bytes(&self) -> usize {
        self.total
    }
}

impl<const ORDER: usize> fmt::Debug for Heap<ORDER> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Heap")
            .field("user", &self.user)
            .field("allocated", &self.allocated)
            .field("total", &self.total)
            .finish()
    }
}

/// A locked version of `Heap`
///
/// # Usage
///
/// Create a locked heap and add a memory region to it:
/// ```
/// use buddy_system_allocator::*;
/// # use core::mem::size_of;
/// let mut heap = LockedHeap::<32>::new();
/// # let space: [usize; 100] = [0; 100];
/// # let begin: usize = space.as_ptr() as usize;
/// # let end: usize = begin + 100 * size_of::<usize>();
/// # let size: usize = 100 * size_of::<usize>();
/// unsafe {
///     heap.lock().init(begin, size);
///     // or
///     heap.lock().add_to_heap(begin, end);
/// }
/// ```
#[cfg(feature = "use_spin")]
pub struct LockedHeap<const ORDER: usize>(Mutex<Heap<ORDER>>);

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> LockedHeap<ORDER> {
    /// Creates an empty heap
    pub const fn new() -> Self {
        LockedHeap(Mutex::new(Heap::<ORDER>::new()))
    }

    /// Creates an empty heap
    pub const fn empty() -> Self {
        LockedHeap(Mutex::new(Heap::<ORDER>::new()))
    }
}

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> Deref for LockedHeap<ORDER> {
    type Target = Mutex<Heap<ORDER>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(feature = "use_spin")]
unsafe impl<const ORDER: usize> GlobalAlloc for LockedHeap<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0
            .lock()
            .alloc(layout)
            .map_or(0 as *mut u8, |allocation| allocation.as_ptr())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0.lock().dealloc(NonNull::new_unchecked(ptr), layout)
    }
}

/// A locked version of `Heap` with rescue before oom
///
/// # Usage
///
/// Create a locked heap:
/// ```
/// use buddy_system_allocator::*;
/// let heap = LockedHeapWithRescue::new(|heap: &mut Heap<32>, layout: &core::alloc::Layout| {});
/// ```
///
/// Before oom, the allocator will try to call rescue function and try for one more time.
#[cfg(feature = "use_spin")]
pub struct LockedHeapWithRescue<const ORDER: usize> {
    inner: Mutex<Heap<ORDER>>,
    rescue: fn(&mut Heap<ORDER>, &Layout),
}

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> LockedHeapWithRescue<ORDER> {
    /// Creates an empty heap
    #[cfg(feature = "const_fn")]
    pub const fn new(rescue: fn(&mut Heap<ORDER>, &Layout)) -> Self {
        LockedHeapWithRescue {
            inner: Mutex::new(Heap::<ORDER>::new()),
            rescue,
        }
    }

    /// Creates an empty heap
    #[cfg(not(feature = "const_fn"))]
    pub fn new(rescue: fn(&mut Heap<ORDER>, &Layout)) -> Self {
        LockedHeapWithRescue {
            inner: Mutex::new(Heap::<ORDER>::new()),
            rescue,
        }
    }
}

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> Deref for LockedHeapWithRescue<ORDER> {
    type Target = Mutex<Heap<ORDER>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(feature = "use_spin")]
unsafe impl<const ORDER: usize> GlobalAlloc for LockedHeapWithRescue<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut inner = self.inner.lock();
        match inner.alloc(layout) {
            Some(allocation) => allocation.as_ptr(),
            None => {
                (self.rescue)(&mut inner, &layout);
                inner
                    .alloc(layout)
                    .map_or(0 as *mut u8, |allocation| allocation.as_ptr())
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.inner
            .lock()
            .dealloc(NonNull::new_unchecked(ptr), layout)
    }
}

pub(crate) fn prev_power_of_two(num: usize) -> usize {
    1 << (8 * (size_of::<usize>()) - num.leading_zeros() as usize - 1)
}
