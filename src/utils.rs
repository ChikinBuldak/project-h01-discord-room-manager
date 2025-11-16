use rand::Rng;

#[allow(dead_code)]
pub fn generate_random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

// =================================================
// Helper Functions
// =================================================

pub fn generate_room_id() -> String {
    let mut rng = rand::rng();

    let num: u64 = rng.random_range(0..1_000_000_000_000);
    format!("{:012}", num)
}
