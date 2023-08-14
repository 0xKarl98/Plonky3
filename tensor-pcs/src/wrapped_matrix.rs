use core::marker::PhantomData;

use p3_matrix::{Matrix, MatrixRows};

pub struct WrappedMatrix<'a, T, M> {
    inner: M,
    wraps: usize,
    _phantom: PhantomData<&'a T>,
}

impl<'a, T, M> WrappedMatrix<'a, T, M>
where
    M: Matrix<T>,
{
    pub fn new(inner: M, wraps: usize) -> Self {
        assert_eq!(inner.height() % wraps, 0);
        Self {
            inner,
            wraps,
            _phantom: PhantomData,
        }
    }
}

impl<'a, T, M> Matrix<T> for WrappedMatrix<'a, T, M>
where
    M: Matrix<T>,
{
    fn width(&self) -> usize {
        self.inner.width() * self.wraps
    }

    fn height(&self) -> usize {
        self.inner.width() / self.wraps
    }
}

impl<'a, T, M> MatrixRows<T> for WrappedMatrix<'a, T, M>
where
    M: MatrixRows< T> + 'a,
{
    type Row = WrappedMatrixRow<'a, T, M>;

    fn row(&self, r: usize) -> Self::Row {
        WrappedMatrixRow {
            wrapped_matrix: self,
            row: r,
            current_iter: self.inner.row(r).into_iter(),
            next_wrap: 1,
        }
    }
}

pub struct WrappedMatrixRow<'a, T, M>
where
    T: 'a,
    M: MatrixRows< T>,
{
    wrapped_matrix: &'a WrappedMatrix<'a, T, M>,
    row: usize,
    current_iter: <M::Row as IntoIterator>::IntoIter,
    next_wrap: usize,
}

impl<'a, T, M> Iterator for WrappedMatrixRow<'a, T, M>
where
    T: 'a,
    M: MatrixRows<T>,
{
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.current_iter.next().or_else(|| {
            (self.next_wrap < self.wrapped_matrix.wraps).then(|| {
                self.current_iter = self
                    .wrapped_matrix
                    .inner
                    .row(self.next_wrap * self.wrapped_matrix.wraps + self.row)
                    .into_iter();
                self.next_wrap += 1;
                self.current_iter.next().unwrap()
            })
        })
    }
}
