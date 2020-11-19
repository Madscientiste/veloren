pub mod building;
mod town;

use self::{
    building::{Building, House, Keep},
    town::{District, Town},
};
use super::SpawnRules;
use crate::{
    column::ColumnSample,
    sim::WorldSim,
    util::{RandomField, Sampler, StructureGen2d},
    site::namegen::NameGen,
    IndexRef,
};
use common::{
    astar::Astar,
    comp::{self, bird_medium, humanoid, object, quadruped_small, Item},
    generation::{ChunkSupplement, EntityInfo},
    path::Path,
    spiral::Spiral2d,
    store::{Id, Store},
    terrain::{Block, BlockKind, SpriteKind, TerrainChunkSize},
    vol::{BaseVol, ReadVol, RectSizedVol, RectVolSize, WriteVol},
};
use fxhash::FxHasher64;
use hashbrown::{HashMap, HashSet};
use rand::prelude::*;
use serde::Deserialize;
use std::{collections::VecDeque, f32, hash::BuildHasherDefault};
use vek::*;

#[derive(Deserialize)]
pub struct Colors {
    pub building: building::Colors,

    pub plot_town_path: (u8, u8, u8),

    pub plot_field_dirt: (u8, u8, u8),
    pub plot_field_mound: (u8, u8, u8),

    pub wall_low: (u8, u8, u8),
    pub wall_high: (u8, u8, u8),

    pub tower_color: (u8, u8, u8),

    pub plot_dirt: (u8, u8, u8),
    pub plot_grass: (u8, u8, u8),
    pub plot_water: (u8, u8, u8),
    pub plot_town: (u8, u8, u8),
}

#[allow(dead_code)]
pub fn gradient(line: [Vec2<f32>; 2]) -> f32 {
    let r = (line[0].y - line[1].y) / (line[0].x - line[1].x);
    if r.is_nan() { 100000.0 } else { r }
}

#[allow(dead_code)]
pub fn intersect(a: [Vec2<f32>; 2], b: [Vec2<f32>; 2]) -> Option<Vec2<f32>> {
    let ma = gradient(a);
    let mb = gradient(b);

    let ca = a[0].y - ma * a[0].x;
    let cb = b[0].y - mb * b[0].x;

    if (ma - mb).abs() < 0.0001 || (ca - cb).abs() < 0.0001 {
        None
    } else {
        let x = (cb - ca) / (ma - mb);
        let y = ma * x + ca;

        Some(Vec2::new(x, y))
    }
}

#[allow(dead_code)]
pub fn center_of(p: [Vec2<f32>; 3]) -> Vec2<f32> {
    let ma = -1.0 / gradient([p[0], p[1]]);
    let mb = -1.0 / gradient([p[1], p[2]]);

    let pa = (p[0] + p[1]) * 0.5;
    let pb = (p[1] + p[2]) * 0.5;

    let ca = pa.y - ma * pa.x;
    let cb = pb.y - mb * pb.x;

    let x = (cb - ca) / (ma - mb);
    let y = ma * x + ca;

    Vec2::new(x, y)
}

impl WorldSim {
    fn can_host_settlement(&self, pos: Vec2<i32>) -> bool {
        self.get(pos)
            .map(|chunk| !chunk.river.is_river() && !chunk.river.is_lake())
            .unwrap_or(false)
            && self
                .get_gradient_approx(pos)
                .map(|grad| grad < 0.75)
                .unwrap_or(false)
    }
}

const AREA_SIZE: u32 = 32;

fn to_tile(e: i32) -> i32 { ((e as f32).div_euclid(AREA_SIZE as f32)).floor() as i32 }

pub enum StructureKind {
    House(Building<House>),
    Keep(Building<Keep>),
}

pub struct Structure {
    kind: StructureKind,
}

impl Structure {
    pub fn bounds_2d(&self) -> Aabr<i32> {
        match &self.kind {
            StructureKind::House(house) => house.bounds_2d(),
            StructureKind::Keep(keep) => keep.bounds_2d(),
        }
    }

    pub fn bounds(&self) -> Aabb<i32> {
        match &self.kind {
            StructureKind::House(house) => house.bounds(),
            StructureKind::Keep(keep) => keep.bounds(),
        }
    }

    pub fn sample(&self, index: IndexRef, rpos: Vec3<i32>) -> Option<Block> {
        match &self.kind {
            StructureKind::House(house) => house.sample(index, rpos),
            StructureKind::Keep(keep) => keep.sample(index, rpos),
        }
    }
}

pub struct Settlement {
    name: String,
    seed: u32,
    origin: Vec2<i32>,
    land: Land,
    farms: Store<Farm>,
    structures: Vec<Structure>,
    town: Option<Town>,
    noise: RandomField,
}

pub struct Farm {
    #[allow(dead_code)]
    base_tile: Vec2<i32>,
}

pub struct GenCtx<'a, R: Rng> {
    sim: Option<&'a WorldSim>,
    rng: &'a mut R,
}

impl Settlement {
    pub fn generate(wpos: Vec2<i32>, sim: Option<&WorldSim>, rng: &mut impl Rng) -> Self {
        let mut ctx = GenCtx { sim, rng };
        let mut this = Self {
            name: NameGen::location(ctx.rng).generate(),
            seed: ctx.rng.gen(),
            origin: wpos,
            land: Land::new(ctx.rng),
            farms: Store::default(),
            structures: Vec::new(),
            town: None,
            noise: RandomField::new(ctx.rng.gen()),
        };

        if let Some(sim) = ctx.sim {
            this.designate_from_world(sim, ctx.rng);
        }

        //this.place_river(rng);

        this.place_farms(&mut ctx);
        this.place_town(&mut ctx);
        //this.place_paths(ctx.rng);
        this.place_buildings(&mut ctx);

        this
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn get_origin(&self) -> Vec2<i32> { self.origin }

    /// Designate hazardous terrain based on world data
    #[allow(clippy::blocks_in_if_conditions)] // TODO: Pending review in #587
    pub fn designate_from_world(&mut self, sim: &WorldSim, rng: &mut impl Rng) {
        let tile_radius = self.radius() as i32 / AREA_SIZE as i32;
        let hazard = self.land.hazard;
        Spiral2d::new()
            .take_while(|tile| tile.map(|e| e.abs()).reduce_max() < tile_radius)
            .for_each(|tile| {
                let wpos = self.origin + tile * AREA_SIZE as i32;

                if (0..4)
                    .map(|x| (0..4).map(move |y| Vec2::new(x, y)))
                    .flatten()
                    .any(|offs| {
                        let wpos = wpos + offs * AREA_SIZE as i32 / 2;
                        let cpos = wpos.map(|e| e.div_euclid(TerrainChunkSize::RECT_SIZE.x as i32));
                        !sim.can_host_settlement(cpos)
                    })
                    || rng.gen_range(0, 16) == 0
                // Randomly consider some tiles inaccessible
                {
                    self.land.set(tile, hazard);
                }
            })
    }

    /// Testing only
    pub fn place_river(&mut self, rng: &mut impl Rng) {
        let river_dir = Vec2::new(rng.gen::<f32>() - 0.5, rng.gen::<f32>() - 0.5).normalized();
        let radius = 500.0 + rng.gen::<f32>().powf(2.0) * 1000.0;
        let river = self.land.new_plot(Plot::Water);
        let river_offs = Vec2::new(rng.gen_range(-3, 4), rng.gen_range(-3, 4));

        for x in (0..100).map(|e| e as f32 / 100.0) {
            let theta0 = x as f32 * f32::consts::PI * 2.0;
            let theta1 = (x + 0.01) as f32 * f32::consts::PI * 2.0;

            let pos0 = (river_dir * radius + Vec2::new(theta0.sin(), theta0.cos()) * radius)
                .map(|e| e.floor() as i32)
                .map(to_tile)
                + river_offs;
            let pos1 = (river_dir * radius + Vec2::new(theta1.sin(), theta1.cos()) * radius)
                .map(|e| e.floor() as i32)
                .map(to_tile)
                + river_offs;

            if pos0.magnitude_squared() > 15i32.pow(2) {
                continue;
            }

            if let Some(path) = self.land.find_path(pos0, pos1, |_, _| 1.0) {
                for pos in path.iter().copied() {
                    self.land.set(pos, river);
                }
            }
        }
    }

    #[allow(clippy::or_fun_call)] // TODO: Pending review in #587
    pub fn place_paths(&mut self, rng: &mut impl Rng) {
        const PATH_COUNT: usize = 6;

        let mut dir = Vec2::zero();
        for _ in 0..PATH_COUNT {
            dir = (Vec2::new(rng.gen::<f32>() - 0.5, rng.gen::<f32>() - 0.5) * 2.0 - dir)
                .try_normalized()
                .unwrap_or_else(Vec2::zero);
            let origin = dir.map(|e| (e * 100.0) as i32);
            let origin = self
                .land
                .find_tile_near(origin, |plot| matches!(plot, Some(&Plot::Field { .. })))
                .unwrap();

            if let Some(path) = self.town.as_ref().and_then(|town| {
                self.land
                    .find_path(origin, town.base_tile, |from, to| match (from, to) {
                        (_, Some(b)) if self.land.plot(b.plot) == &Plot::Dirt => 0.0,
                        (_, Some(b)) if self.land.plot(b.plot) == &Plot::Water => 20.0,
                        (_, Some(b)) if self.land.plot(b.plot) == &Plot::Hazard => 50.0,
                        (Some(a), Some(b)) if a.contains(WayKind::Wall) => {
                            if b.contains(WayKind::Wall) {
                                1000.0
                            } else {
                                10.0
                            }
                        },
                        (Some(_), Some(_)) => 1.0,
                        _ => 1000.0,
                    })
            }) {
                let path = path.iter().copied().collect::<Vec<_>>();
                self.land.write_path(&path, WayKind::Path, |_| true, false);
            }
        }
    }

    pub fn place_town(&mut self, ctx: &mut GenCtx<impl Rng>) {
        const PLOT_COUNT: usize = 3;

        let mut origin = Vec2::new(ctx.rng.gen_range(-2, 3), ctx.rng.gen_range(-2, 3));

        for i in 0..PLOT_COUNT {
            if let Some(base_tile) = self.land.find_tile_near(origin, |plot| match plot {
                Some(Plot::Field { .. }) => true,
                Some(Plot::Dirt) => true,
                _ => false,
            }) {
                // self.land
                //     .plot_at_mut(base_tile)
                //     .map(|plot| *plot = Plot::Town { district: None });

                if i == 0 {
                    let town = Town::generate(self.origin, base_tile, ctx);

                    for (id, district) in town.districts().iter() {
                        let district_plot =
                            self.land.plots.insert(Plot::Town { district: Some(id) });

                        for x in district.aabr.min.x..district.aabr.max.x {
                            for y in district.aabr.min.y..district.aabr.max.y {
                                if !matches!(self.land.plot_at(Vec2::new(x, y)), Some(Plot::Hazard))
                                {
                                    self.land.set(Vec2::new(x, y), district_plot);
                                }
                            }
                        }
                    }

                    self.town = Some(town);
                    origin = base_tile;
                }
            }
        }

        // Boundary wall
        /*
        let spokes = CARDINALS
            .iter()
            .filter_map(|dir| {
                self.land.find_tile_dir(origin, *dir, |plot| match plot {
                    Some(Plot::Water) => false,
                    Some(Plot::Town) => false,
                    _ => true,
                })
            })
            .collect::<Vec<_>>();
        let mut wall_path = Vec::new();
        for i in 0..spokes.len() {
            self.land
                .find_path(spokes[i], spokes[(i + 1) % spokes.len()], |_, to| match to
                    .map(|to| self.land.plot(to.plot))
                {
                    Some(Plot::Hazard) => 200.0,
                    Some(Plot::Water) => 40.0,
                    Some(Plot::Town) => 10000.0,
                    _ => 10.0,
                })
                .map(|path| wall_path.extend(path.iter().copied()));
        }
        let grass = self.land.new_plot(Plot::Grass);
        let buildable = |plot: &Plot| match plot {
            Plot::Water => false,
            _ => true,
        };
        for pos in wall_path.iter() {
            if self.land.tile_at(*pos).is_none() {
                self.land.set(*pos, grass);
            }
            if self.land.plot_at(*pos).copied().filter(buildable).is_some() {
                self.land
                    .tile_at_mut(*pos)
                    .map(|tile| tile.tower = Some(Tower::Wall));
            }
        }
        if wall_path.len() > 0 {
            wall_path.push(wall_path[0]);
        }
        self.land
            .write_path(&wall_path, WayKind::Wall, buildable, true);
        */
    }

    pub fn place_buildings(&mut self, ctx: &mut GenCtx<impl Rng>) {
        let town_center = if let Some(town) = self.town.as_ref() {
            town.base_tile
        } else {
            return;
        };

        for tile in Spiral2d::new()
            .map(|offs| town_center + offs)
            .take(16usize.pow(2))
        {
            // This is a stupid way to decide how to place buildings
            for i in 0..ctx.rng.gen_range(2, 5) {
                for _ in 0..25 {
                    let house_pos = tile.map(|e| e * AREA_SIZE as i32 + AREA_SIZE as i32 / 2)
                        + Vec2::<i32>::zero().map(|_| {
                            ctx.rng
                                .gen_range(-(AREA_SIZE as i32) / 4, AREA_SIZE as i32 / 4)
                        });

                    let tile_pos = house_pos.map(|e| e.div_euclid(AREA_SIZE as i32));
                    if self
                        .land
                        .tile_at(tile_pos)
                        .map(|t| t.contains(WayKind::Path))
                        .unwrap_or(true)
                        || ctx
                            .sim
                            .and_then(|sim| sim.get_nearest_path(self.origin + house_pos))
                            .map(|(dist, _, _, _)| dist < 28.0)
                            .unwrap_or(false)
                    {
                        continue;
                    }

                    let alt = if let Some(Plot::Town { district }) = self.land.plot_at(tile_pos) {
                        district
                            .and_then(|d| self.town.as_ref().map(|t| t.districts().get(d)))
                            .map(|d| d.alt)
                            .filter(|_| false) // Temporary
                            .unwrap_or_else(|| {
                                ctx.sim
                                    .and_then(|sim| sim.get_alt_approx(self.origin + house_pos))
                                    .unwrap_or(0.0)
                                    .ceil() as i32
                            })
                    } else {
                        continue;
                    };

                    let structure = Structure {
                        kind: if tile == town_center && i == 0 {
                            StructureKind::Keep(Building::<Keep>::generate(
                                ctx.rng,
                                Vec3::new(house_pos.x, house_pos.y, alt),
                            ))
                        } else {
                            StructureKind::House(Building::<House>::generate(
                                ctx.rng,
                                Vec3::new(house_pos.x, house_pos.y, alt),
                            ))
                        },
                    };

                    let bounds = structure.bounds_2d();

                    // Check for collision with other structures
                    if self
                        .structures
                        .iter()
                        .any(|s| s.bounds_2d().collides_with_aabr(bounds))
                    {
                        continue;
                    }

                    self.structures.push(structure);
                    break;
                }
            }
        }
    }

    pub fn place_farms(&mut self, ctx: &mut GenCtx<impl Rng>) {
        const FARM_COUNT: usize = 6;
        const FIELDS_PER_FARM: usize = 5;

        for _ in 0..FARM_COUNT {
            if let Some(base_tile) = self
                .land
                .find_tile_near(Vec2::zero(), |plot| plot.is_none())
            {
                // Farm
                //let farmhouse = self.land.new_plot(Plot::Dirt);
                //self.land.set(base_tile, farmhouse);

                // Farmhouses
                // for _ in 0..ctx.rng.gen_range(1, 3) {
                //     let house_pos = base_tile.map(|e| e * AREA_SIZE as i32 + AREA_SIZE as i32
                // / 2)         + Vec2::new(ctx.rng.gen_range(-16, 16),
                // ctx.rng.gen_range(-16, 16));

                //     self.structures.push(Structure {
                //         kind: StructureKind::House(HouseBuilding::generate(ctx.rng,
                // Vec3::new(             house_pos.x,
                //             house_pos.y,
                //             ctx.sim
                //                 .and_then(|sim| sim.get_alt_approx(self.origin + house_pos))
                //                 .unwrap_or(0.0)
                //                 .ceil() as i32,
                //         ))),
                //     });
                // }

                // Fields
                let farmland = self.farms.insert(Farm { base_tile });
                for _ in 0..FIELDS_PER_FARM {
                    self.place_field(farmland, base_tile, ctx.rng);
                }
            }
        }
    }

    pub fn place_field(
        &mut self,
        farm: Id<Farm>,
        origin: Vec2<i32>,
        rng: &mut impl Rng,
    ) -> Option<Id<Plot>> {
        const MAX_FIELD_SIZE: usize = 24;

        if let Some(center) = self.land.find_tile_near(origin, |plot| plot.is_none()) {
            let field = self.land.new_plot(Plot::Field {
                farm,
                seed: rng.gen(),
                crop: match rng.gen_range(0, 8) {
                    0 => Crop::Corn,
                    1 => Crop::Wheat,
                    2 => Crop::Cabbage,
                    3 => Crop::Pumpkin,
                    4 => Crop::Flax,
                    5 => Crop::Carrot,
                    6 => Crop::Tomato,
                    7 => Crop::Radish,
                    _ => Crop::Sunflower,
                },
            });
            let tiles =
                self.land
                    .grow_from(center, rng.gen_range(5, MAX_FIELD_SIZE), rng, |plot| {
                        plot.is_none()
                    });
            for pos in tiles.into_iter() {
                self.land.set(pos, field);
            }
            Some(field)
        } else {
            None
        }
    }

    pub fn radius(&self) -> f32 { 400.0 }

    #[allow(clippy::needless_update)] // TODO: Pending review in #587
    pub fn spawn_rules(&self, wpos: Vec2<i32>) -> SpawnRules {
        SpawnRules {
            trees: self
                .land
                .get_at_block(wpos - self.origin)
                .plot
                .map(|p| matches!(p, Plot::Hazard))
                .unwrap_or(true),
            ..SpawnRules::default()
        }
    }

    #[allow(clippy::identity_op)] // TODO: Pending review in #587
    #[allow(clippy::modulo_one)] // TODO: Pending review in #587
    pub fn apply_to<'a>(
        &'a self,
        index: IndexRef,
        wpos2d: Vec2<i32>,
        mut get_column: impl FnMut(Vec2<i32>) -> Option<&'a ColumnSample<'a>>,
        vol: &mut (impl BaseVol<Vox = Block> + RectSizedVol + ReadVol + WriteVol),
    ) {
        let colors = &index.colors.site.settlement;

        for y in 0..vol.size_xy().y as i32 {
            for x in 0..vol.size_xy().x as i32 {
                let offs = Vec2::new(x, y);

                let wpos2d = wpos2d + offs;
                let rpos = wpos2d - self.origin;

                // Sample terrain
                let col_sample = if let Some(col_sample) = get_column(offs) {
                    col_sample
                } else {
                    continue;
                };
                let land_surface_z = col_sample.riverless_alt.floor() as i32;
                let mut surface_z = land_surface_z;

                // Sample settlement
                let sample = self.land.get_at_block(rpos);

                let noisy_color = move |col: Rgb<u8>, factor: u32| {
                    let nz = self.noise.get(Vec3::new(wpos2d.x, wpos2d.y, surface_z));
                    col.map(|e| {
                        (e as u32 + nz % (factor * 2))
                            .saturating_sub(factor)
                            .min(255) as u8
                    })
                };

                // District alt
                if let Some(Plot::Town { district }) = sample.plot {
                    if let Some(d) = district
                        .and_then(|d| self.town.as_ref().map(|t| t.districts().get(d)))
                        .filter(|_| false)
                    // Temporary
                    {
                        let other = self
                            .land
                            .plot_at(sample.second_closest)
                            .and_then(|p| match p {
                                Plot::Town { district } => *district,
                                _ => None,
                            })
                            .and_then(|d| {
                                self.town.as_ref().map(|t| t.districts().get(d).alt as f32)
                            })
                            .filter(|_| false)
                            .unwrap_or(surface_z as f32);
                        surface_z = Lerp::lerp(
                            (other + d.alt as f32) / 2.0,
                            d.alt as f32,
                            (1.25 * sample.edge_dist / (d.alt as f32 - other).abs()).min(1.0),
                        ) as i32;
                    }
                }

                {
                    let mut surface_sprite = None;

                    let roll =
                        |seed, n| self.noise.get(Vec3::new(wpos2d.x, wpos2d.y, seed * 5)) % n;

                    let color = match sample.plot {
                        Some(Plot::Dirt) => Some(colors.plot_dirt.into()),
                        Some(Plot::Grass) => Some(colors.plot_grass.into()),
                        Some(Plot::Water) => Some(colors.plot_water.into()),
                        //Some(Plot::Town { district }) => None,
                        Some(Plot::Town { .. }) => {
                            if let Some((_, path_nearest, _, _)) = col_sample.path {
                                let path_dir = (path_nearest - wpos2d.map(|e| e as f32))
                                    .rotated_z(f32::consts::PI / 2.0)
                                    .normalized();
                                let is_lamp = if path_dir.x.abs() > path_dir.y.abs() {
                                    wpos2d.x as f32 % 30.0 / path_dir.dot(Vec2::unit_y()).abs()
                                        <= 1.0
                                } else {
                                    (wpos2d.y as f32 + 10.0) % 30.0
                                        / path_dir.dot(Vec2::unit_x()).abs()
                                        <= 1.0
                                };
                                if (col_sample.path.map(|(dist, _, _, _)| dist > 6.0 && dist < 7.0).unwrap_or(false) && is_lamp) //roll(0, 50) == 0)
                                    || (roll(0, 2000) == 0 && col_sample.path.map(|(dist, _, _, _)| dist > 20.0).unwrap_or(true))
                                {
                                    surface_sprite = Some(SpriteKind::StreetLamp);
                                }
                            }

                            Some(Rgb::from(colors.plot_town_path).map2(
                                Rgb::iota(),
                                |e: u8, i: i32| {
                                    e.saturating_add(
                                        (self.noise.get(Vec3::new(wpos2d.x, wpos2d.y, i * 5)) % 1)
                                            as u8,
                                    )
                                    .saturating_sub(8)
                                },
                            ))
                        },
                        Some(Plot::Field { seed, crop, .. }) => {
                            let furrow_dirs = [
                                Vec2::new(1, 0),
                                Vec2::new(0, 1),
                                Vec2::new(1, 1),
                                Vec2::new(-1, 1),
                            ];
                            let furrow_dir = furrow_dirs[*seed as usize % furrow_dirs.len()];
                            let in_furrow = (wpos2d * furrow_dir).sum().rem_euclid(5) < 2;

                            let dirt = Rgb::<u8>::from(colors.plot_field_dirt).map(|e| {
                                e + (self.noise.get(Vec3::broadcast((seed % 4096 + 0) as i32)) % 32)
                                    as u8
                            });
                            let mound = Rgb::<u8>::from(colors.plot_field_mound)
                                .map(|e| e + roll(0, 8) as u8)
                                .map(|e| {
                                    e + (self.noise.get(Vec3::broadcast((seed % 4096 + 1) as i32))
                                        % 32) as u8
                                });

                            if in_furrow {
                                if roll(0, 5) == 0 {
                                    surface_sprite = match crop {
                                        Crop::Corn => Some(SpriteKind::Corn),
                                        Crop::Wheat if roll(1, 2) == 0 => {
                                            Some(SpriteKind::WheatYellow)
                                        },
                                        Crop::Wheat => Some(SpriteKind::WheatGreen),
                                        Crop::Cabbage if roll(2, 2) == 0 => {
                                            Some(SpriteKind::Cabbage)
                                        },
                                        Crop::Pumpkin if roll(3, 2) == 0 => {
                                            Some(SpriteKind::Pumpkin)
                                        },
                                        Crop::Flax if roll(4, 2) == 0 => Some(SpriteKind::Flax),
                                        Crop::Carrot if roll(5, 2) == 0 => Some(SpriteKind::Carrot),
                                        Crop::Tomato if roll(6, 2) == 0 => Some(SpriteKind::Tomato),
                                        Crop::Radish if roll(7, 2) == 0 => Some(SpriteKind::Radish),
                                        Crop::Turnip if roll(8, 2) == 0 => Some(SpriteKind::Turnip),
                                        Crop::Sunflower => Some(SpriteKind::Sunflower),
                                        _ => surface_sprite,
                                    }
                                    .or_else(|| {
                                        if roll(9, 400) == 0 {
                                            Some(SpriteKind::Scarecrow)
                                        } else {
                                            None
                                        }
                                    });
                                }
                            } else if roll(0, 20) == 0 {
                                surface_sprite = Some(SpriteKind::ShortGrass);
                            } else if roll(1, 30) == 0 {
                                surface_sprite = Some(SpriteKind::MediumGrass);
                            }

                            Some(if in_furrow { dirt } else { mound })
                        },
                        _ => None,
                    };

                    if let Some(color) = color {
                        let is_path = col_sample
                            .path
                            .map(|(dist, _, path, _)| dist < path.width)
                            .unwrap_or(false);

                        if col_sample.water_dist.map(|dist| dist > 2.0).unwrap_or(true) && !is_path
                        {
                            let diff = (surface_z - land_surface_z).abs();

                            for z in -8 - diff..4 + diff {
                                let pos = Vec3::new(offs.x, offs.y, surface_z + z);
                                let block = if let Ok(&block) = vol.get(pos) {
                                    // TODO: Figure out whether extra filters are needed.
                                    block
                                } else {
                                    break;
                                };

                                if let (0, Some(sprite)) = (z, surface_sprite) {
                                    let _ = vol.set(
                                        pos,
                                        // TODO: Make more principled.
                                        if block.is_fluid() {
                                            block.with_sprite(sprite)
                                        } else {
                                            Block::air(sprite)
                                        },
                                    );
                                } else if z >= 0 {
                                    if block.kind() != BlockKind::Water {
                                        let _ = vol.set(pos, Block::air(SpriteKind::Empty));
                                    }
                                } else {
                                    let _ = vol.set(
                                        pos,
                                        Block::new(BlockKind::Earth, noisy_color(color, 4)),
                                    );
                                }
                            }
                        }
                    }
                }

                // Walls
                if let Some((WayKind::Wall, dist, _)) = sample.way {
                    let color = Lerp::lerp(
                        Rgb::<u8>::from(colors.wall_low).map(i32::from),
                        Rgb::<u8>::from(colors.wall_high).map(i32::from),
                        (RandomField::new(0).get(wpos2d.into()) % 256) as f32 / 256.0,
                    )
                    .map(|e| (e % 256) as u8);

                    let z_offset = if let Some(water_dist) = col_sample.water_dist {
                        // Water gate
                        ((water_dist.max(0.0) * 0.45).min(f32::consts::PI).cos() + 1.0) * 4.0
                    } else {
                        0.0
                    } as i32;

                    for z in z_offset..12 {
                        if dist / WayKind::Wall.width() < ((1.0 - z as f32 / 12.0) * 2.0).min(1.0) {
                            let _ = vol.set(
                                Vec3::new(offs.x, offs.y, surface_z + z),
                                Block::new(BlockKind::Wood, color),
                            );
                        }
                    }
                }

                // Towers
                if let Some((Tower::Wall, _pos)) = sample.tower {
                    for z in -2..16 {
                        let _ = vol.set(
                            Vec3::new(offs.x, offs.y, surface_z + z),
                            Block::new(BlockKind::Rock, colors.tower_color.into()),
                        );
                    }
                }
            }
        }

        // Apply structures
        for structure in &self.structures {
            let bounds = structure.bounds_2d();

            // Skip this structure if it's not near this chunk
            if !bounds.collides_with_aabr(Aabr {
                min: wpos2d - self.origin,
                max: wpos2d - self.origin + vol.size_xy().map(|e| e as i32 + 1),
            }) {
                continue;
            }

            let bounds = structure.bounds();

            for x in bounds.min.x..bounds.max.x + 1 {
                for y in bounds.min.y..bounds.max.y + 1 {
                    let col = if let Some(col) = get_column(self.origin + Vec2::new(x, y) - wpos2d)
                    {
                        col
                    } else {
                        continue;
                    };

                    for z in bounds.min.z.min(col.alt.floor() as i32 - 1)..bounds.max.z + 1 {
                        let rpos = Vec3::new(x, y, z);
                        let wpos = Vec3::from(self.origin) + rpos;
                        let coffs = wpos - Vec3::from(wpos2d);

                        if let Some(block) = structure.sample(index, rpos) {
                            let _ = vol.set(coffs, block);
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::eval_order_dependence)] // TODO: Pending review in #587
    pub fn apply_supplement<'a>(
        &'a self,
        // NOTE: Used only for dynamic elements like chests and entities!
        dynamic_rng: &mut impl Rng,
        wpos2d: Vec2<i32>,
        mut get_column: impl FnMut(Vec2<i32>) -> Option<&'a ColumnSample<'a>>,
        supplement: &mut ChunkSupplement,
    ) {
        for y in 0..TerrainChunkSize::RECT_SIZE.y as i32 {
            for x in 0..TerrainChunkSize::RECT_SIZE.x as i32 {
                let offs = Vec2::new(x, y);

                let wpos2d = wpos2d + offs;
                let rpos = wpos2d - self.origin;

                // Sample terrain
                let col_sample = if let Some(col_sample) = get_column(offs) {
                    col_sample
                } else {
                    continue;
                };

                let sample = self.land.get_at_block(rpos);

                let entity_wpos = Vec3::new(wpos2d.x as f32, wpos2d.y as f32, col_sample.alt + 3.0);

                if matches!(sample.plot, Some(Plot::Town { .. }))
                    && RandomField::new(self.seed).chance(Vec3::from(wpos2d), 1.0 / (50.0 * 40.0))
                {
                    let is_human: bool;
                    let is_dummy =
                        RandomField::new(self.seed + 1).chance(Vec3::from(wpos2d), 1.0 / 15.0);
                    let entity = EntityInfo::at(entity_wpos)
                        .with_body(match dynamic_rng.gen_range(0, 5) {
                            _ if is_dummy => {
                                is_human = false;
                                object::Body::TrainingDummy.into()
                            },
                            0 => {
                                let species = match dynamic_rng.gen_range(0, 3) {
                                    0 => quadruped_small::Species::Pig,
                                    1 => quadruped_small::Species::Sheep,
                                    _ => quadruped_small::Species::Cat,
                                };
                                is_human = false;
                                comp::Body::QuadrupedSmall(quadruped_small::Body::random_with(
                                    dynamic_rng,
                                    &species,
                                ))
                            },
                            1 => {
                                let species = match dynamic_rng.gen_range(0, 4) {
                                    0 => bird_medium::Species::Duck,
                                    1 => bird_medium::Species::Chicken,
                                    2 => bird_medium::Species::Goose,
                                    _ => bird_medium::Species::Peacock,
                                };
                                is_human = false;
                                comp::Body::BirdMedium(bird_medium::Body::random_with(
                                    dynamic_rng,
                                    &species,
                                ))
                            },
                            _ => {
                                is_human = true;
                                comp::Body::Humanoid(humanoid::Body::random())
                            },
                        })
                        .with_agency(!is_dummy)
                        .with_alignment(if is_dummy {
                            comp::Alignment::Passive
                        } else if is_human {
                            comp::Alignment::Npc
                        } else {
                            comp::Alignment::Tame
                        })
                        .do_if(is_human && dynamic_rng.gen(), |entity| {
                            entity.with_main_tool(Item::new_from_asset_expect(
                                match dynamic_rng.gen_range(0, 7) {
                                    0 => "common.items.npc_weapons.tool.broom",
                                    1 => "common.items.npc_weapons.tool.hoe",
                                    2 => "common.items.npc_weapons.tool.pickaxe",
                                    3 => "common.items.npc_weapons.tool.pitchfork",
                                    4 => "common.items.npc_weapons.tool.rake",
                                    5 => "common.items.npc_weapons.tool.shovel-0",
                                    _ => "common.items.npc_weapons.tool.shovel-1",
                                    //_ => "common.items.npc_weapons.bow.starter_bow", TODO: Re-Add this when we have a better way of distributing npc_weapons here
                                },
                            ))
                        })
                        .do_if(is_dummy, |e| e.with_name("Training Dummy"))
                        .do_if(!is_dummy, |e| e.with_automatic_name());

                    supplement.add_entity(entity);
                }
            }
        }
    }

    pub fn get_color(&self, index: IndexRef, pos: Vec2<i32>) -> Option<Rgb<u8>> {
        let colors = &index.colors.site.settlement;

        let sample = self.land.get_at_block(pos);

        match sample.plot {
            Some(Plot::Dirt) => return Some(colors.plot_dirt.into()),
            Some(Plot::Grass) => return Some(colors.plot_grass.into()),
            Some(Plot::Water) => return Some(colors.plot_water.into()),
            Some(Plot::Town { .. }) => {
                return Some(
                    Rgb::from(colors.plot_town).map2(Rgb::iota(), |e: u8, i: i32| {
                        e.saturating_add(
                            (self.noise.get(Vec3::new(pos.x, pos.y, i * 5)) % 16) as u8,
                        )
                        .saturating_sub(8)
                    }),
                );
            },
            Some(Plot::Field { seed, .. }) => {
                let furrow_dirs = [
                    Vec2::new(1, 0),
                    Vec2::new(0, 1),
                    Vec2::new(1, 1),
                    Vec2::new(-1, 1),
                ];
                let furrow_dir = furrow_dirs[*seed as usize % furrow_dirs.len()];
                let furrow = (pos * furrow_dir).sum().rem_euclid(6) < 3;
                // NOTE: Very hard to understand how to make this dynamically configurable.  The
                // base values can easily cause the others to go out of range, and there's some
                // weird scaling going on.  For now, we just let these remain hardcoded.
                //
                // FIXME: Rewrite this so that validity is not so heavily dependent on the exact
                // color values.
                return Some(Rgb::new(
                    if furrow {
                        100
                    } else {
                        32 + seed.to_le_bytes()[0] % 64
                    },
                    64 + seed.to_le_bytes()[1] % 128,
                    16 + seed.to_le_bytes()[2] % 32,
                ));
            },
            _ => {},
        }

        None
    }
}

#[derive(Copy, Clone, PartialEq)]
pub enum Crop {
    Corn,
    Wheat,
    Cabbage,
    Pumpkin,
    Flax,
    Carrot,
    Tomato,
    Radish,
    Turnip,
    Sunflower,
}

// NOTE: No support for struct variants in make_case_elim yet, unfortunately, so
// we can't use it.
#[derive(Copy, Clone, PartialEq)]
pub enum Plot {
    Hazard,
    Dirt,
    Grass,
    Water,
    Town {
        district: Option<Id<District>>,
    },
    Field {
        farm: Id<Farm>,
        seed: u32,
        crop: Crop,
    },
}

const CARDINALS: [Vec2<i32>; 4] = [
    Vec2::new(0, 1),
    Vec2::new(1, 0),
    Vec2::new(0, -1),
    Vec2::new(-1, 0),
];

#[derive(Copy, Clone, PartialEq)]
pub enum WayKind {
    Path,
    #[allow(dead_code)]
    Wall,
}

impl WayKind {
    pub fn width(&self) -> f32 {
        match self {
            WayKind::Path => 4.0,
            WayKind::Wall => 3.0,
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
pub enum Tower {
    #[allow(dead_code)]
    Wall,
}

impl Tower {
    pub fn radius(&self) -> f32 {
        match self {
            Tower::Wall => 6.0,
        }
    }
}

pub struct Tile {
    plot: Id<Plot>,
    ways: [Option<WayKind>; 4],
    tower: Option<Tower>,
}

impl Tile {
    pub fn contains(&self, kind: WayKind) -> bool { self.ways.iter().any(|way| way == &Some(kind)) }
}

#[derive(Default)]
pub struct Sample<'a> {
    plot: Option<&'a Plot>,
    way: Option<(&'a WayKind, f32, Vec2<f32>)>,
    tower: Option<(&'a Tower, Vec2<i32>)>,
    edge_dist: f32,
    second_closest: Vec2<i32>,
}

pub struct Land {
    /// We use this hasher (FxHasher64) because
    /// (1) we need determinism across computers (ruling out AAHash);
    /// (2) we don't care about DDOS attacks (ruling out SipHash);
    /// (3) we have 8-byte keys (for which FxHash is fastest).
    tiles: HashMap<Vec2<i32>, Tile, BuildHasherDefault<FxHasher64>>,
    plots: Store<Plot>,
    sampler_warp: StructureGen2d,
    hazard: Id<Plot>,
}

impl Land {
    pub fn new(rng: &mut impl Rng) -> Self {
        let mut plots = Store::default();
        let hazard = plots.insert(Plot::Hazard);
        Self {
            tiles: HashMap::default(),
            plots,
            sampler_warp: StructureGen2d::new(rng.gen(), AREA_SIZE, AREA_SIZE * 2 / 5),
            hazard,
        }
    }

    pub fn get_at_block(&self, pos: Vec2<i32>) -> Sample {
        let mut sample = Sample::default();

        let neighbors = self.sampler_warp.get(pos);
        let closest = neighbors
            .iter()
            .min_by_key(|(center, _)| center.distance_squared(pos))
            .unwrap()
            .0;
        let second_closest = neighbors
            .iter()
            .filter(|(center, _)| *center != closest)
            .min_by_key(|(center, _)| center.distance_squared(pos))
            .unwrap()
            .0;
        sample.second_closest = second_closest.map(to_tile);
        sample.edge_dist = (second_closest - pos).map(|e| e as f32).magnitude()
            - (closest - pos).map(|e| e as f32).magnitude();

        let center_tile = self.tile_at(neighbors[4].0.map(to_tile));

        if let Some(tower) = center_tile.and_then(|tile| tile.tower.as_ref()) {
            if (neighbors[4].0.distance_squared(pos) as f32) < tower.radius().powf(2.0) {
                sample.tower = Some((tower, neighbors[4].0));
            }
        }

        for (i, _) in CARDINALS.iter().enumerate() {
            let map = [1, 5, 7, 3];
            let line = LineSegment2 {
                start: neighbors[4].0.map(|e| e as f32),
                end: neighbors[map[i]].0.map(|e| e as f32),
            };
            if let Some(way) = center_tile.and_then(|tile| tile.ways[i].as_ref()) {
                let proj_point = line.projected_point(pos.map(|e| e as f32));
                let dist = proj_point.distance(pos.map(|e| e as f32));
                if dist < way.width() {
                    sample.way = sample
                        .way
                        .filter(|(_, d, _)| *d < dist)
                        .or(Some((way, dist, proj_point)));
                }
            }
        }

        sample.plot = self.plot_at(closest.map(to_tile));

        sample
    }

    pub fn tile_at(&self, pos: Vec2<i32>) -> Option<&Tile> { self.tiles.get(&pos) }

    #[allow(dead_code)]
    pub fn tile_at_mut(&mut self, pos: Vec2<i32>) -> Option<&mut Tile> { self.tiles.get_mut(&pos) }

    pub fn plot(&self, id: Id<Plot>) -> &Plot { self.plots.get(id) }

    pub fn plot_at(&self, pos: Vec2<i32>) -> Option<&Plot> {
        self.tiles.get(&pos).map(|tile| self.plots.get(tile.plot))
    }

    #[allow(dead_code)]
    pub fn plot_at_mut(&mut self, pos: Vec2<i32>) -> Option<&mut Plot> {
        self.tiles
            .get(&pos)
            .map(|tile| tile.plot)
            .map(move |plot| self.plots.get_mut(plot))
    }

    pub fn set(&mut self, pos: Vec2<i32>, plot: Id<Plot>) {
        self.tiles.insert(pos, Tile {
            plot,
            ways: [None; 4],
            tower: None,
        });
    }

    fn find_tile_near(
        &self,
        origin: Vec2<i32>,
        mut match_fn: impl FnMut(Option<&Plot>) -> bool,
    ) -> Option<Vec2<i32>> {
        Spiral2d::new()
            .map(|pos| origin + pos)
            .find(|pos| match_fn(self.plot_at(*pos)))
    }

    #[allow(dead_code)]
    fn find_tile_dir(
        &self,
        origin: Vec2<i32>,
        dir: Vec2<i32>,
        mut match_fn: impl FnMut(Option<&Plot>) -> bool,
    ) -> Option<Vec2<i32>> {
        (0..)
            .map(|i| origin + dir * i)
            .find(|pos| match_fn(self.plot_at(*pos)))
    }

    fn find_path(
        &self,
        origin: Vec2<i32>,
        dest: Vec2<i32>,
        mut path_cost_fn: impl FnMut(Option<&Tile>, Option<&Tile>) -> f32,
    ) -> Option<Path<Vec2<i32>>> {
        let heuristic = |pos: &Vec2<i32>| (pos - dest).map(|e| e as f32).magnitude();
        let neighbors = |pos: &Vec2<i32>| {
            let pos = *pos;
            CARDINALS.iter().map(move |dir| pos + *dir)
        };
        let transition =
            |from: &Vec2<i32>, to: &Vec2<i32>| path_cost_fn(self.tile_at(*from), self.tile_at(*to));
        let satisfied = |pos: &Vec2<i32>| *pos == dest;

        // We use this hasher (FxHasher64) because
        // (1) we don't care about DDOS attacks (ruling out SipHash);
        // (2) we don't care about determinism across computers (we could use AAHash);
        // (3) we have 8-byte keys (for which FxHash is fastest).
        Astar::new(
            250,
            origin,
            heuristic,
            BuildHasherDefault::<FxHasher64>::default(),
        )
        .poll(250, heuristic, neighbors, transition, satisfied)
        .into_path()
    }

    /// We use this hasher (FxHasher64) because
    /// (1) we don't care about DDOS attacks (ruling out SipHash);
    /// (2) we care about determinism across computers (ruling out AAHash);
    /// (3) we have 8-byte keys (for which FxHash is fastest).
    fn grow_from(
        &self,
        start: Vec2<i32>,
        max_size: usize,
        _rng: &mut impl Rng,
        mut match_fn: impl FnMut(Option<&Plot>) -> bool,
    ) -> HashSet<Vec2<i32>, BuildHasherDefault<FxHasher64>> {
        let mut open = VecDeque::new();
        open.push_back(start);
        // We use this hasher (FxHasher64) because
        // (1) we don't care about DDOS attacks (ruling out SipHash);
        // (2) we care about determinism across computers (ruling out AAHash);
        // (3) we have 8-byte keys (for which FxHash is fastest).
        let mut closed = HashSet::with_hasher(BuildHasherDefault::<FxHasher64>::default());

        while open.len() + closed.len() < max_size {
            let next_pos = if let Some(next_pos) = open.pop_front() {
                closed.insert(next_pos);
                next_pos
            } else {
                break;
            };

            let dirs = [
                Vec2::new(1, 0),
                Vec2::new(-1, 0),
                Vec2::new(0, 1),
                Vec2::new(0, -1),
            ];

            for dir in dirs.iter() {
                let neighbor = next_pos + dir;
                if !closed.contains(&neighbor) && match_fn(self.plot_at(neighbor)) {
                    open.push_back(neighbor);
                }
            }
        }

        closed.into_iter().chain(open.into_iter()).collect()
    }

    fn write_path(
        &mut self,
        tiles: &[Vec2<i32>],
        kind: WayKind,
        mut permit_fn: impl FnMut(&Plot) -> bool,
        overwrite: bool,
    ) {
        for tiles in tiles.windows(2) {
            let dir = tiles[1] - tiles[0];
            let idx = if dir.y > 0 {
                1
            } else if dir.x > 0 {
                2
            } else if dir.y < 0 {
                3
            } else if dir.x < 0 {
                0
            } else {
                continue;
            };
            if self.tile_at(tiles[0]).is_none() {
                self.set(tiles[0], self.hazard);
            }
            let plots = &self.plots;

            self.tiles
                .get_mut(&tiles[1])
                .filter(|tile| permit_fn(plots.get(tile.plot)))
                .map(|tile| {
                    if overwrite || tile.ways[(idx + 2) % 4].is_none() {
                        tile.ways[(idx + 2) % 4] = Some(kind);
                    }
                });
            self.tiles
                .get_mut(&tiles[0])
                .filter(|tile| permit_fn(plots.get(tile.plot)))
                .map(|tile| {
                    if overwrite || tile.ways[idx].is_none() {
                        tile.ways[idx] = Some(kind);
                    }
                });
        }
    }

    pub fn new_plot(&mut self, plot: Plot) -> Id<Plot> { self.plots.insert(plot) }
}
