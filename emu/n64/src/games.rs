//! Which save memory each game has, which nothing on the cartridge tells,
//! so emulators and flashcarts keep a list of games. This is
//! N64FlashcartMenu's (src/menu/rom_info.c, at commit 7fcef17d92ee, from
//! github.com/Polprzewodnikowy/N64FlashcartMenu), with only the save
//! memory kept and the games without one left out. A game is looked up by
//! its code in the header (bytes 3B-3E: a letter for the kind of media,
//! two for the game and one for the region; three letters match every
//! region) and, for a few, its version (byte 3F); the first line that
//! matches wins. Before that, homebrew whose header has the code "ED" says
//! itself, in the top four bits of the version, and a few cartridges are
//! told by their check code (bytes 10-17).

use crate::save::Kind::{self, Eeprom4k, Eeprom16k, Flash, Sram, SramBanked};

/// The save memory of the game a ROM's header is of.
pub fn save_kind(header: &[u8]) -> Kind {
    let (code, version) = (&header[0x3b..0x3f], header[0x3f]);
    if &code[1..3] == b"ED" {
        return match version >> 4 {
            1 => Eeprom4k,
            2 => Eeprom16k,
            3 => Sram,
            4 => SramBanked,
            5 => Flash,
            _ => Kind::None,
        };
    }
    let check = u64::from_be_bytes(header[0x10..0x18].try_into().unwrap());
    if let Some(&(_, kind)) = CHECK_CODES.iter().find(|&&(c, _)| c == check) {
        return kind;
    }
    GAMES
        .iter()
        .find(|&&(id, v, _)| code.starts_with(id) && (v < 0 || v == i16::from(version)))
        .map_or(Kind::None, |&(_, _, kind)| kind)
}

// The lists, made from N64FlashcartMenu's with the names it gives the games.

const CHECK_CODES: &[(u64, Kind)] = &[
    (0x000000004cbc3b56, Sram), // DMTJ 64DD cartridge conversion
    (0x0dd4ababb5a2a91e, Eeprom16k), // DK Retail kiosk demo
    (0xeb85ebc9596682af, Flash), // Doubutsu Banchou
    (0x9a746ebf2802ea99, Eeprom4k), // Toon panic
    (0x21548ca921548ca9, Eeprom4k), // Mini racers
    (0xbc9b2cc34ed04da5, Flash), // Starcraft 64 [Prototype 2000]
    (0x5d40ed2c10d6abcf, Eeprom4k), // Viewpoint 2064
    (0x7280e03f497689ba, Eeprom4k), // Viewpoint 2064 [ENG patch]
    (0xcdb8b4d08832352d, Sram), // Jet Force Gemini [USA CRACK]
    (0xb66e0f7c2709c22f, Sram), // Jet Force Gemini [PAL CRACK]
    (0xce84793d27ecc1ad, Sram), // Donkey kong 64 [USA CRACK]
    (0x1f95caaa047fc22a, Sram), // Donkey kong 64 [PAL CRACK]
    (0xe3ff09dfcae4b0ed, Sram), // Banjo tooie [USA CRACK]
];

const GAMES: &[(&[u8], i16, Kind)] = &[
    (b"NK4J", 0, Sram), // Kirby 64: The Crystal Shards [Hoshi no Kirby 64 (J)]
    (b"NK4J", 1, Sram), // Kirby 64: The Crystal Shards [Hoshi no Kirby 64 (J)]
    (b"NK4", -1, Eeprom4k), // Kirby 64: The Crystal Shards [Hoshi no Kirby 64 (J)]
    (b"NSMJ", 3, Eeprom4k), // Super Mario 64 Shindou Edition
    (b"NSM", -1, Eeprom4k), // Super Mario 64
    (b"NWRJ", 2, Eeprom4k), // Wave Race 64 Shindou Edition
    (b"NWR", -1, Eeprom4k), // Wave Race 64
    (b"N3HJ", -1, Sram), // Ganbare! Nippon! Olympics 2000
    (b"ND3J", -1, Eeprom16k), // Akumajou Dracula Mokushiroku (J)
    (b"ND4J", -1, Eeprom16k), // Akumajou Dracula Mokushiroku Gaiden: Legend of Cornell (J)
    (b"NDKJ", -1, Eeprom4k), // Dark Rift [Space Dynamites (J)]
    (b"NPDJ", -1, Eeprom16k), // Perfect Dark (J)
    (b"NPD", -1, Eeprom16k), // Perfect Dark
    (b"NSVE", -1, Eeprom4k), // Space Station Silicon Valley
    (b"NSV", -1, Eeprom4k), // Space Station Silicon Valley
    (b"NWTJ", -1, Eeprom4k), // Wetrix
    (b"CLB", -1, Eeprom4k), // Mario Party (NTSC)
    (b"NAB", -1, Eeprom4k), // Air Boarder 64
    (b"NAD", -1, Eeprom4k), // Worms Armageddon (U)
    (b"NAG", -1, Eeprom4k), // AeroGauge
    (b"NB6", -1, Eeprom4k), // Super B-Daman: Battle Phoenix 64
    (b"NBC", -1, Eeprom4k), // Blast Corps
    (b"NBD", -1, Eeprom4k), // Bomberman Hero [Mirian Ojo o Sukue! (J)]
    (b"NBH", -1, Eeprom4k), // Body Harvest
    (b"NBK", -1, Eeprom4k), // Banjo-Kazooie [Banjo to Kazooie no Daiboken (J)]
    (b"NBM", -1, Eeprom4k), // Bomberman 64 [Baku Bomberman (J)]
    (b"NBN", -1, Eeprom4k), // Bakuretsu Muteki Bangaioh
    (b"NBV", -1, Eeprom4k), // Bomberman 64: The Second Attack! [Baku Bomberman 2 (J)]
    (b"NCG", -1, Eeprom4k), // Choro Q 64 II - Hacha Mecha Grand Prix Race (J)
    (b"NCH", -1, Eeprom4k), // Chopper Attack
    (b"NCR", -1, Eeprom4k), // Penny Racers [Choro Q 64 (J)]
    (b"NCT", -1, Eeprom4k), // Chameleon Twist
    (b"NCU", -1, Eeprom4k), // Cruis'n USA
    (b"NCX", -1, Eeprom4k), // Custom Robo
    (b"NDQ", -1, Eeprom4k), // Disney's Donald Duck - Goin' Quackers [Quack Attack (E)]
    (b"NDR", -1, Eeprom4k), // Doraemon: Nobita to 3tsu no Seireiseki
    (b"NDU", -1, Eeprom4k), // Duck Dodgers starring Daffy Duck
    (b"NDY", -1, Eeprom4k), // Diddy Kong Racing
    (b"NEA", -1, Eeprom4k), // PGA European Tour
    (b"NER", -1, Eeprom4k), // Aero Fighters Assault [Sonic Wings Assault (J)]
    (b"NF2", -1, Eeprom4k), // F-1 World Grand Prix II
    (b"NFG", -1, Eeprom4k), // Fighter Destiny 2
    (b"NFH", -1, Eeprom4k), // In-Fisherman Bass Hunter 64
    (b"NFW", -1, Eeprom4k), // F-1 World Grand Prix
    (b"NFX", -1, Eeprom4k), // Star Fox 64 [Lylat Wars (E)]
    (b"NFY", -1, Eeprom4k), // Kakutou Denshou: F-Cup Maniax
    (b"NGE", -1, Eeprom4k), // GoldenEye 007
    (b"NGL", -1, Eeprom4k), // Getter Love!!
    (b"NGU", -1, Eeprom4k), // Tsumi to Batsu: Hoshi no Keishousha (Sin and Punishment)
    (b"NGV", -1, Eeprom4k), // Glover
    (b"NHA", -1, Eeprom4k), // Bomberman 64: Arcade Edition (J)
    (b"NHF", -1, Eeprom4k), // 64 Hanafuda: Tenshi no Yakusoku
    (b"NHP", -1, Eeprom4k), // Heiwa Pachinko World 64
    (b"NIC", -1, Eeprom4k), // Indy Racing 2000
    (b"NIJ", -1, Eeprom4k), // Indiana Jones and the Infernal Machine
    (b"NIR", -1, Eeprom4k), // Utchan Nanchan no Hono no Challenger: Denryuu Ira Ira Bou
    (b"NJK", -1, Eeprom4k), // Viewpoint 2064 (Final Prototype) (J)
    (b"NJM", -1, Eeprom4k), // Earthworm Jim 3D
    (b"NK2", -1, Eeprom4k), // Snowboard Kids 2 [Chou Snobow Kids (J)]
    (b"NKA", -1, Eeprom4k), // Fighters Destiny [Fighting Cup (J)]
    (b"NKI", -1, Eeprom4k), // Killer Instinct Gold
    (b"NKT", -1, Eeprom4k), // Mario Kart 64
    (b"NLB", -1, Eeprom4k), // Mario Party (PAL)
    (b"NLL", -1, Eeprom4k), // Last Legion UX
    (b"NLR", -1, Eeprom4k), // Lode Runner 3-D
    (b"NMG", -1, Eeprom4k), // Monaco Grand Prix [Racing Simulation 2 (G)]
    (b"NMI", -1, Eeprom4k), // Mission: Impossible
    (b"NML", -1, Eeprom4k), // Mickey's Speedway USA [Mickey no Racing Challenge USA (J)]
    (b"NMO", -1, Eeprom4k), // Monopoly
    (b"NMR", -1, Eeprom4k), // Multi-Racing Championship
    (b"NMS", -1, Eeprom4k), // Morita Shougi 64
    (b"NMU", -1, Eeprom4k), // Big Mountain 2000
    (b"NMW", -1, Eeprom4k), // Mario Party 2
    (b"NMZ", -1, Eeprom4k), // Zool - Majou Tsukai Densetsu (J)
    (b"NN6", -1, Eeprom4k), // Dr. Mario 64
    (b"NNA", -1, Eeprom4k), // Star Wars Episode I: Battle for Naboo
    (b"NOS", -1, Eeprom4k), // 64 Oozumou
    (b"NO2", -1, Eeprom4k), // 64 Oozumou 2
    (b"NP2", -1, Eeprom4k), // Chou Kuukan Night Pro Yakyuu King 2 (J)
    (b"NPG", -1, Eeprom4k), // Hey You, Pikachu! [Pikachu Genki Dechu (J)]
    (b"NPT", -1, Eeprom4k), // Puyo Puyon Party
    (b"NPW", -1, Eeprom4k), // Pilotwings 64
    (b"NPY", -1, Eeprom4k), // Puyo Puyo Sun 64
    (b"NRA", -1, Eeprom4k), // Rally '99 (J)
    (b"NRC", -1, Eeprom4k), // Top Gear Overdrive
    (b"NRS", -1, Eeprom4k), // Star Wars: Rogue Squadron [Shutsugeki! Rogue Chuutai (J)]
    (b"NS3", -1, Eeprom4k), // AI Shougi 3
    (b"NS6", -1, Eeprom4k), // Star Soldier: Vanishing Earth
    (b"NSA", -1, Eeprom4k), // Sonic Wings Assault (J)
    (b"NSC", -1, Eeprom4k), // Starshot: Space Circus Fever
    (b"NSN", -1, Eeprom4k), // Snow Speeder (J)
    (b"NSS", -1, Eeprom4k), // Super Robot Spirits
    (b"NSU", -1, Eeprom4k), // Rocket: Robot on Wheels
    (b"NSW", -1, Eeprom4k), // Star Wars: Shadows of the Empire [Teikoku no Kage (J)]
    (b"NT6", -1, Eeprom4k), // Tetris 64
    (b"NTB", -1, Eeprom4k), // Transformers: Beast Wars Metals 64 (J)
    (b"NTC", -1, Eeprom4k), // 64 Trump Collection
    (b"NTJ", -1, Eeprom4k), // Tom & Jerry in Fists of Fury
    (b"NTM", -1, Eeprom4k), // Mischief Makers [Yuke Yuke!! Trouble Makers (J)]
    (b"NTN", -1, Eeprom4k), // All Star Tennis '99
    (b"NTP", -1, Eeprom4k), // Tetrisphere
    (b"NTR", -1, Eeprom4k), // Top Gear Rally (J + E)
    (b"NTW", -1, Eeprom4k), // 64 de Hakken!! Tamagotchi
    (b"NTX", -1, Eeprom4k), // Taz Express
    (b"NVL", -1, Eeprom4k), // V-Rally Edition '99
    (b"NVY", -1, Eeprom4k), // V-Rally Edition '99 (J)
    (b"NWC", -1, Eeprom4k), // Wild Choppers
    (b"NWQ", -1, Eeprom4k), // Rally Challenge 2000
    (b"NWU", -1, Eeprom4k), // Worms Armageddon (E)
    (b"NXO", -1, Eeprom4k), // Cruis'n Exotica
    (b"NYK", -1, Eeprom4k), // Yakouchuu II: Satsujin Kouro
    (b"N3D", -1, Eeprom16k), // Doraemon 3: Nobita no Machi SOS!
    (b"NB7", -1, Eeprom16k), // Banjo-Tooie [Banjo to Kazooie no Daiboken 2 (J)]
    (b"NCW", -1, Eeprom16k), // Cruis'n World
    (b"NCZ", -1, Eeprom16k), // Custom Robo V2
    (b"ND2", -1, Eeprom16k), // Doraemon 2: Nobita to Hikari no Shinden
    (b"ND6", -1, Eeprom16k), // Densha de Go! 64
    (b"NDO", -1, Eeprom16k), // Donkey Kong 64
    (b"NEP", -1, Eeprom16k), // Star Wars Episode I: Racer
    (b"NEV", -1, Eeprom16k), // Neon Genesis Evangelion
    (b"NFU", -1, Eeprom16k), // Conker's Bad Fur Day
    (b"NGC", -1, Eeprom16k), // GT 64: Championship Edition
    (b"NGT", -1, Eeprom16k), // City Tour GrandPrix - Zen Nihon GT Senshuken
    (b"NIM", -1, Eeprom16k), // Ide Yosuke no Mahjong Juku
    (b"NM8", -1, Eeprom16k), // Mario Tennis
    (b"NMV", -1, Eeprom16k), // Mario Party 3
    (b"NMX", -1, Eeprom16k), // Excitebike 64
    (b"NNB", -1, Eeprom16k), // Kobe Bryant in NBA Courtside
    (b"NPP", -1, Eeprom16k), // Parlor! Pro 64: Pachinko Jikki Simulation Game
    (b"NR7", -1, Eeprom16k), // Robot Poncots 64: 7tsu no Umi no Caramel
    (b"NRZ", -1, Eeprom16k), // Ridge Racer 64
    (b"NUB", -1, Eeprom16k), // PD Ultraman Battle Collection 64
    (b"NYS", -1, Eeprom16k), // Yoshi's Story
    (b"CFZ", -1, Sram), // F-Zero X (J)
    (b"CPS", -1, Sram), // Pocket Monsters Stadium (J)
    (b"CZL", -1, Sram), // Legend of Zelda: Ocarina of Time [Zelda no Densetsu - Toki no Ocarina (J)]
    (b"NA2", -1, Sram), // Virtual Pro Wrestling 2
    (b"NAL", -1, Sram), // Super Smash Bros. [Nintendo All-Star! Dairantou Smash Brothers (J)]
    (b"NB5", -1, Sram), // Biohazard 2 (J)
    (b"NDD", -1, Sram), // 64DD Conversion Rom
    (b"NFZ", -1, Sram), // F-Zero X (U + E)
    (b"NG6", -1, Sram), // Ganmare Goemon: Dero Dero Douchuu Obake Tenkomori
    (b"NGP", -1, Sram), // Goemon: Mononoke Sugoroku
    (b"NHY", -1, Sram), // Hybrid Heaven (J)
    (b"NIB", -1, Sram), // Itoi Shigesato no Bass Tsuri No. 1 Kettei Ban!
    (b"NJ5", -1, Sram), // Jikkyou Powerful Pro Yakyuu 5
    (b"NJG", -1, Sram), // Jinsei Game 64
    (b"NKG", -1, Sram), // Major League Baseball featuring Ken Griffey Jr.
    (b"NMF", -1, Sram), // Mario Golf 64
    (b"NOB", -1, Sram), // Ogre Battle 64: Person of Lordly Caliber
    (b"NP4", -1, Sram), // Jikkyou Powerful Pro Yakyuu 4
    (b"NP6", -1, Sram), // Jikkyou Powerful Pro Yakyuu 6
    (b"NPA", -1, Sram), // Jikkyou Powerful Pro Yakyuu 2000
    (b"NPE", -1, Sram), // Jikkyou Powerful Pro Yakyuu Basic Ban 2001
    (b"NPM", -1, Sram), // Premier Manager 64
    (b"NPS", -1, Sram), // Jikkyou J.League 1999: Perfect Striker 2
    (b"NRE", -1, Sram), // Resident Evil 2
    (b"NRI", -1, Sram), // New Tetris, The
    (b"NS4", -1, Sram), // Super Robot Taisen 64
    (b"NSI", -1, Sram), // Fushigi no Dungeon: Fuurai no Shiren 2
    (b"NT3", -1, Sram), // Shin Nihon Pro Wrestling - Toukon Road 2 - The Next Generation (J)
    (b"NTE", -1, Sram), // 1080 Snowboarding
    (b"NUM", -1, Sram), // Nushi Zuri 64: Shiokaze ni Notte
    (b"NUT", -1, Sram), // Nushi Zuri 64
    (b"NVB", -1, Sram), // Bass Rush - ECOGEAR PowerWorm Championship (J)
    (b"NVP", -1, Sram), // Virtual Pro Wrestling 64
    (b"NW2", -1, Sram), // WCW-nWo Revenge
    (b"NWL", -1, Sram), // Waialae Country Club: True Golf Classics
    (b"NWX", -1, Sram), // WWF WrestleMania 2000
    (b"NYW", -1, Sram), // Harvest Moon 64
    (b"NZL", -1, Sram), // Legend of Zelda: Ocarina of Time (E)
    (b"CDZ", -1, SramBanked), // Dezaemon 3D
    (b"CP2", -1, Flash), // Pocket Monsters Stadium 2 (J)
    (b"NAF", -1, Flash), // Doubutsu no Mori
    (b"NCC", -1, Flash), // Command & Conquer
    (b"NCV", -1, Flash), // Cubivore (Translation)
    (b"NCK", -1, Flash), // NBA Courtside 2 featuring Kobe Bryant
    (b"NDA", -1, Flash), // Derby Stallion 64
    (b"NDP", -1, Flash), // Dinosaur Planet (Unlicensed)
    (b"NJF", -1, Flash), // Jet Force Gemini [Star Twins (J)]
    (b"NKJ", -1, Flash), // Ken Griffey Jr.'s Slugfest
    (b"NM6", -1, Flash), // Mega Man 64
    (b"NMQ", -1, Flash), // Paper Mario
    (b"NPF", -1, Flash), // Pokemon Snap [Pocket Monsters Snap (J)]
    (b"NPN", -1, Flash), // Pokemon Puzzle League
    (b"NPO", -1, Flash), // Pokemon Stadium
    (b"NRH", -1, Flash), // Rockman Dash - Hagane no Boukenshin (J)
    (b"NSQ", -1, Flash), // StarCraft 64
    (b"NT9", -1, Flash), // Tigger's Honey Hunt
    (b"NW4", -1, Flash), // WWF No Mercy
    (b"NZS", -1, Flash), // Legend of Zelda: Majora's Mask [Zelda no Densetsu - Mujura no Kamen (J)]
    (b"NP3", -1, Flash), // Pokemon Stadium 2 [Pocket Monsters Stadium - Kin Gin (J)]
];

#[cfg(test)]
mod tests {
    use super::*;

    fn header(code: &[u8; 4], version: u8) -> Vec<u8> {
        let mut header = vec![0; 0x40];
        header[0x3b..0x3f].copy_from_slice(code);
        header[0x3f] = version;
        header
    }

    #[test]
    fn games() {
        assert_eq!(save_kind(&header(b"NSME", 0)), Eeprom4k);
        assert_eq!(save_kind(&header(b"NDOE", 0)), Eeprom16k);
        assert_eq!(save_kind(&header(b"CZLE", 2)), Sram);
        assert_eq!(save_kind(&header(b"NMQE", 0)), Flash);
        // Kirby 64: SRAM in the first two Japanese versions, EEPROM after.
        assert_eq!(save_kind(&header(b"NK4J", 1)), Sram);
        assert_eq!(save_kind(&header(b"NK4J", 2)), Eeprom4k);
        assert_eq!(save_kind(&header(b"NK4E", 0)), Eeprom4k);
        assert_eq!(save_kind(&header(b"NXXE", 0)), Kind::None);
        assert_eq!(save_kind(&header(b"NEDE", 0x30)), Sram);
    }
}
