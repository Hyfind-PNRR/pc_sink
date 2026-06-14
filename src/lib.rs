pub mod models;


fn add(x: usize, y: usize) -> usize {
    x + y
}



#[cfg(test)]
mod lib_test {
    use crate::add;

    #[test]
    fn test_add() {
        assert_eq!(add(1, 2), 3)
    }
}