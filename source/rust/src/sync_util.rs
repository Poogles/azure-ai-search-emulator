//! Lock-acquisition helpers shared by the stateful modules.

/// Acquires a read lock on an [`std::sync::RwLock`], recovering the guard
/// when the lock is poisoned. A poisoned lock means a previous holder
/// panicked; the guarded state is still usable, so the emulator continues
/// rather than failing every subsequent request.
pub fn read_unpoisoned<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Acquires a write lock on an [`std::sync::RwLock`], recovering the guard
/// when the lock is poisoned (see [`read_unpoisoned`]).
pub fn write_unpoisoned<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
