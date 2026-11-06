pub(crate) trait HeapSize {
    fn heap_size(&self) -> usize;

    fn retained_size(&self) -> usize {
        std::mem::size_of_val(self).saturating_add(self.heap_size())
    }
}

impl HeapSize for String {
    fn heap_size(&self) -> usize {
        self.capacity()
    }
}

impl<T: HeapSize> HeapSize for Option<T> {
    fn heap_size(&self) -> usize {
        self.as_ref().map_or(0, HeapSize::heap_size)
    }
}

impl<T: HeapSize> HeapSize for Vec<T> {
    fn heap_size(&self) -> usize {
        self.iter().fold(
            self.capacity().saturating_mul(std::mem::size_of::<T>()),
            |size, value| size.saturating_add(value.heap_size()),
        )
    }
}
