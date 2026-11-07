//! Script-level checks of the 3D world: part and attachment CFrame views, Workspace.Terrain, the
//! spatial queries and the calls reported as world events.

use std::{cell::RefCell, rc::Rc};

use mlua::prelude::*;

use super::WorldEvent;
use crate::{
    Engine, Options,
    lua::{self as engine_lua, globals::CallOutcome},
};

fn start() -> (Lua, Rc<Engine>) {
    let engine = Engine::new(Options::default()).expect("engine");
    let lua = Lua::new();
    engine_lua::start(&lua, &engine).expect("start");
    (lua, engine)
}

/// Runs `source` as a script; panics with the script's error.
fn run(lua: &Lua, engine: &Rc<Engine>, source: &str) {
    let script = {
        let mut state = engine.state.borrow_mut();
        let class = engine.reflection.class_id("ModuleScript").unwrap();
        crate::instance::create_named(&mut state, &engine.reflection, class, "Test")
    };
    let env = engine_lua::modules::environment(lua, engine, script).expect("environment");
    let function = lua.load(source).set_name("=Test").set_environment(env).into_function().expect("compile");
    let call = engine_lua::globals::call(lua, engine, function, LuaMultiValue::new()).expect("call");
    if let CallOutcome::Failed(message, trace) = call.outcome().unwrap() {
        panic!("test script failed: {message}\n{}", trace.unwrap_or_default());
    }
}

fn check(source: &str) {
    let (lua, engine) = start();
    run(&lua, &engine, source);
}

#[test]
fn part_cframe_views() {
    check(
        r#"
        local function near(a, b) return (a - b).Magnitude < 1e-3 end
        local Part = Instance.new("Part")
        Part.CFrame = CFrame.new(1, 2, 3) * CFrame.Angles(0, math.rad(90), 0)
        assert(near(Part.Position, Vector3.new(1, 2, 3)), tostring(Part.Position))
        assert(near(Part.Orientation, Vector3.new(0, 90, 0)), tostring(Part.Orientation))
        -- (XYZ angles lose an axis at 90 degrees about Y: the last one reads 0)
        assert(near(Part.Rotation, Vector3.new(0, 90, 0)), tostring(Part.Rotation))
        Part.Position = Vector3.new(5, 6, 7)
        assert(near(Part.CFrame.Position, Vector3.new(5, 6, 7)))
        assert(near(Part.CFrame.LookVector, Vector3.new(-1, 0, 0)), "rotation kept")
        Part.Orientation = Vector3.new(30, 0, 0)
        assert(near(Part.Position, Vector3.new(5, 6, 7)), "position kept")
        assert(near(Part.Orientation, Vector3.new(30, 0, 0)), tostring(Part.Orientation))
        Part.Rotation = Vector3.new(10, 20, 30)
        assert(near(Part.Rotation, Vector3.new(10, 20, 30)), tostring(Part.Rotation))
        assert(near(Part.CFrame.Position, Vector3.new(5, 6, 7)))
        -- the views report their changes
        local Seen = {}
        Part:GetPropertyChangedSignal("Position"):Connect(function() table.insert(Seen, "Position") end)
        Part.CFrame = CFrame.new(0, 10, 0)
        task.wait()
        assert(Seen[1] == "Position", "Position changed with CFrame")
        "#,
    );
}

#[test]
fn attachment_world_members() {
    check(
        r#"
        local function near(a, b) return (a - b).Magnitude < 1e-3 end
        local Part = Instance.new("Part")
        Part.CFrame = CFrame.new(10, 0, 0) * CFrame.Angles(0, math.rad(90), 0)
        local Attachment = Instance.new("Attachment")
        Attachment.Position = Vector3.new(0, 0, -2)
        Attachment.Parent = Part
        -- -Z of a part turned 90 degrees about Y is -X
        assert(near(Attachment.WorldPosition, Vector3.new(8, 0, 0)), tostring(Attachment.WorldPosition))
        assert(near(Attachment.WorldCFrame.Position, Vector3.new(8, 0, 0)))
        assert(near(Attachment.WorldAxis, Part.CFrame.RightVector))
        Attachment.WorldPosition = Vector3.new(10, 5, 0)
        assert(near(Attachment.Position, Vector3.new(0, 5, 0)), tostring(Attachment.Position))
        Attachment.WorldCFrame = CFrame.new(12, 0, 0)
        assert(near(Attachment.WorldPosition, Vector3.new(12, 0, 0)))
        -- an attachment outside a part is in world space
        local Loose = Instance.new("Attachment")
        Loose.CFrame = CFrame.new(1, 2, 3)
        assert(near(Loose.WorldPosition, Vector3.new(1, 2, 3)))
        Loose.Axis = Vector3.new(0, 0, 1)
        assert(near(Loose.Axis, Vector3.new(0, 0, 1)) and near(Loose.SecondaryAxis, Vector3.new(0, 1, 0)))
        "#,
    );
}

/// Measured in Studio (2026-10-17, the capture Studio): an attachment hangs from its nearest part,
/// attachment or model ancestor past Folders, Configurations and Accessories; a model's frame is its
/// pivot, an empty Model's or a WorldModel's the identity.
#[test]
fn attachments_hang_past_folders() {
    check(
        r#"
        local function near(a, b) return (a - b).Magnitude < 1e-3 end
        local Root = Instance.new("Folder"); Root.Parent = workspace
        local Part = Instance.new("Part"); Part.Anchored = true
        Part.CFrame = CFrame.new(100, 50, 20) * CFrame.Angles(0, math.rad(90), 0); Part.Parent = Root
        local function Att(Parent, Name)
            local A = Instance.new("Attachment"); A.Name = Name; A.CFrame = CFrame.new(1, 2, 3); A.Parent = Parent; return A
        end
        local function In(Class, Parent) local I = Instance.new(Class); I.Parent = Parent; return I end
        local Folder = In("Folder", Part)
        local UnderFolder = Att(Folder, "UnderFolder")
        assert(near(UnderFolder.WorldPosition, Vector3.new(103, 52, 19)), tostring(UnderFolder.WorldPosition))
        assert(near(Att(In("Folder", Folder), "Two").WorldPosition, Vector3.new(103, 52, 19)))
        assert(near(Att(In("Configuration", Part), "Config").WorldPosition, Vector3.new(103, 52, 19)))
        assert(near(Att(In("Accessory", Part), "Accessory").WorldPosition, Vector3.new(103, 52, 19)))
        local Nested = Instance.new("Attachment"); Nested.CFrame = CFrame.new(0, 0, 5); Nested.Parent = UnderFolder
        assert(near(Nested.WorldPosition, Vector3.new(108, 52, 19)), tostring(Nested.WorldPosition))
        local Empty = Att(In("Model", Part), "EmptyModel")
        assert(near(Empty.WorldPosition, Vector3.new(1, 2, 3)), tostring(Empty.WorldPosition) .. " " .. tostring(Empty.Parent:GetPivot().Position))
        local InWorldModel = Att(In("WorldModel", Part), "WorldModel")
        assert(near(InWorldModel.WorldPosition, Vector3.new(1, 2, 3)), tostring(InWorldModel.WorldPosition))
        assert(near(Att(In("Folder", Root), "NoPart").WorldPosition, Vector3.new(1, 2, 3)))
        local Model = In("Model", Root)
        local Inside = Instance.new("Part"); Inside.Anchored = true; Inside.CFrame = CFrame.new(-40, 10, 0); Inside.Parent = Model
        local OnModel = Att(Model, "OnModel")
        assert(near(OnModel.WorldPosition, Vector3.new(-39, 12, 3)), tostring(OnModel.WorldPosition))
        Model.WorldPivot = CFrame.new(500, 0, 0)
        assert(near(OnModel.WorldPosition, Vector3.new(501, 2, 3)), tostring(OnModel.WorldPosition))
        assert(near(Att(In("Folder", Model), "FolderInModel").WorldPosition, Vector3.new(501, 2, 3)))
        Part.CFrame += Vector3.new(0, 100, 0)
        assert(near(UnderFolder.WorldPosition, Vector3.new(103, 152, 19)) and near(Nested.WorldPosition, Vector3.new(108, 152, 19)))
        UnderFolder.WorldPosition = Vector3.new(100, 150, 20)
        assert(near(UnderFolder.Position, Vector3.zero), tostring(UnderFolder.Position))
        "#,
    );
}

#[test]
fn workspace_terrain_and_service_names() {
    check(
        r#"
        assert(workspace.Terrain:IsA("Terrain") and workspace:FindFirstChildOfClass("Terrain") == workspace.Terrain and workspace.Terrain.Parent == workspace)
        assert(game["Run Service"] == game:GetService("RunService"))
        local Settings = UserSettings():GetService("UserGameSettings")
        assert(Settings.SavedQualityLevel == Enum.SavedQualitySetting.Automatic and Settings.SavedQualityLevel.Value == 0)
        assert(UserSettings():GetService("UserGameSettings") == Settings and Settings.Parent == UserSettings())
        assert(UserSettings().GameSettings == Settings and Settings.Name == "GameSettings")
        -- what Roblox's PlayerModule calls on it
        Settings:SetCameraYInvertVisible()
        assert(Settings:GetCameraYInvert() == false and Settings:InFullScreen() == false)
        Settings.FullscreenChanged:Connect(function() end)
        -- deprecated CFrame members still answer
        local Frame = CFrame.new(1, 2, 3)
        assert(Frame.p == Frame.Position and Frame:inverse() == Frame:Inverse() and Frame.lookVector == Frame.LookVector)
        local Size = Vector2.new(3, 4)
        assert(Size.x == 3 and Size.y == 4 and Size.magnitude == 5 and Size.unit == Size.Unit and Size + Size == Vector2.new(6, 8))
        "#,
    );
}

#[test]
fn repeated_raycasts_see_what_changed_between_them() {
    check(
        r#"
        local Box = Instance.new("Part")
        Box.Anchored = true
        Box.Size = Vector3.new(4, 4, 4)
        Box.CFrame = CFrame.new(0, 2, 0)
        Box.Parent = workspace
        local function down(x) return workspace:Raycast(Vector3.new(x, 20, 0), Vector3.new(0, -40, 0)) end
        -- the same rays again find the same part
        assert(down(0).Instance == Box and down(0).Instance == Box and down(10) == nil)
        -- moved, between two rays of one frame
        Box.CFrame = CFrame.new(10, 2, 0)
        assert(down(0) == nil and down(10).Instance == Box)
        -- another parented, then CanQuery turned off
        local Other = Instance.new("Part")
        Other.Size = Vector3.new(4, 4, 4)
        Other.CFrame = CFrame.new(0, 2, 0)
        Other.Parent = workspace
        assert(down(0).Instance == Other)
        Other.CanQuery = false
        assert(down(0) == nil)
        -- unparented
        Box.Parent = nil
        assert(down(10) == nil)
        "#,
    );
}

#[test]
fn raycasts_hit_the_nearest_admitted_part() {
    check(
        r#"
        local function near(a, b) return math.abs(a - b) < 1e-3 end
        local Floor = Instance.new("Part")
        Floor.Name = "Floor"
        Floor.Anchored = true
        Floor.Size = Vector3.new(100, 2, 100)
        Floor.CFrame = CFrame.new(0, -1, 0)
        Floor.Parent = workspace
        local Box = Instance.new("Part")
        Box.Name = "Box"
        Box.Size = Vector3.new(4, 4, 4)
        Box.CFrame = CFrame.new(0, 2, 0) * CFrame.Angles(0, math.rad(45), 0)
        Box.Parent = workspace
        local Ball = Instance.new("Part")
        Ball.Name = "Ball"
        Ball.Shape = Enum.PartType.Ball
        Ball.Size = Vector3.new(4, 4, 4)
        Ball.CFrame = CFrame.new(20, 2, 0)
        Ball.Parent = workspace
        local Ghost = Instance.new("Part")
        Ghost.CanCollide = false
        Ghost.CanQuery = false
        Ghost.Size = Vector3.new(10, 1, 10)
        Ghost.CFrame = CFrame.new(0, 10, 0)
        Ghost.Parent = workspace

        -- down onto the box's top face (the CanQuery-off slab above is not hit)
        local Hit = workspace:Raycast(Vector3.new(0, 20, 0), Vector3.new(0, -40, 0))
        assert(typeof(Hit) == "RaycastResult" and Hit.Instance == Box, Hit and Hit.Instance.Name)
        assert(near(Hit.Position.Y, 4) and near(Hit.Normal.Y, 1) and near(Hit.Distance, 16))
        assert(Hit.Material == Enum.Material.Plastic)
        -- excluded, then only the floor included
        local Params = RaycastParams.new()
        Params.FilterDescendantsInstances = {Box}
        Hit = workspace:Raycast(Vector3.new(0, 20, 0), Vector3.new(0, -40, 0), Params)
        assert(Hit.Instance == Floor and near(Hit.Position.Y, 0))
        Params.FilterType = Enum.RaycastFilterType.Include
        Params.FilterDescendantsInstances = {Floor}
        assert(workspace:Raycast(Vector3.new(0, 20, 0), Vector3.new(0, -40, 0), Params).Instance == Floor)
        -- too short, and starting inside a part
        assert(workspace:Raycast(Vector3.new(0, 20, 0), Vector3.new(0, -10, 0)) == nil)
        assert(workspace:Raycast(Vector3.new(0, 2, 0), Vector3.new(0, 10, 0)) == nil)
        -- a ball by its sphere: from the side, through its surface
        Hit = workspace:Raycast(Vector3.new(30, 2, 0), Vector3.new(-20, 0, 0))
        assert(Hit.Instance == Ball and near(Hit.Position.X, 22) and near(Hit.Normal.X, 1), Hit and tostring(Hit.Position))
        -- a ray missing the ball's sphere but inside its box corner misses
        assert(workspace:Raycast(Vector3.new(30, 3.8, 1.8), Vector3.new(-20, 0, 0)) == nil)
        -- legacy FindPartOnRay
        local Part, Position, Normal = workspace:FindPartOnRayWithIgnoreList(Ray.new(Vector3.new(0, 20, 0), Vector3.new(0, -40, 0)), {Box})
        assert(Part == Floor and near(Position.Y, 0) and near(Normal.Y, 1))
        local Nothing, End = workspace:FindPartOnRay(Ray.new(Vector3.new(0, 50, 0), Vector3.new(0, 5, 0)))
        assert(Nothing == nil and near(End.Y, 55))
        "#,
    );
}

/// RaycastParams / OverlapParams:AddToFilter(Instance | {Instance}) adds to FilterDescendantsInstances
/// (documented; effect modules' shockwaves call it).
#[test]
fn add_to_filter_extends_the_filter() {
    check(
        r#"
        local function Part(Name, Y)
            local Made = Instance.new("Part")
            Made.Name, Made.Anchored, Made.Size, Made.CFrame = Name, true, Vector3.new(10, 1, 10), CFrame.new(0, Y, 0)
            Made.Parent = workspace
            return Made
        end
        local Top, Middle, Bottom = Part("Top", 6), Part("Middle", 3), Part("Bottom", 0)
        local Down = Vector3.new(0, -20, 0)
        local Params = RaycastParams.new()
        local Assigned = {}
        Params.FilterDescendantsInstances = Assigned
        Params:AddToFilter(Top)
        assert(#Assigned == 0, "AddToFilter changed the table the script assigned")
        assert(workspace:Raycast(Vector3.new(0, 10, 0), Down, Params).Instance == Middle, "one instance")
        Params:AddToFilter({Middle})
        local List = Params.FilterDescendantsInstances
        assert(#List == 2 and List[1] == Top and List[2] == Middle, "the list")
        assert(workspace:Raycast(Vector3.new(0, 10, 0), Down, Params).Instance == Bottom, "an array")
        assert(not pcall(function() Params:AddToFilter(5) end), "a number is not an instance")
        local Overlap = OverlapParams.new()
        Overlap:AddToFilter({Top, Bottom})
        local Parts = workspace:GetPartBoundsInBox(CFrame.new(0, 3, 0), Vector3.new(4, 10, 4), Overlap)
        assert(#Parts == 1 and Parts[1] == Middle, "OverlapParams")
        "#,
    );
}

/// RaycastParams / OverlapParams ExcludeInstances and IncludeInstances (`{Instance}?`, nil until
/// assigned; Roblox's spatial query filters of April 2026, devforum.roblox.com/t/4563575 and the
/// reference): the part, then each ancestor in turn, and the first either list names decides,
/// ExcludeInstances first; with none named IncludeInstances = nil lets a part through and `{}` does
/// not. Every query honours them; with FilterDescendantsInstances set too both filters apply (how
/// Roblox combines them is not documented: unmeasured). A list assigned or read back is a copy.
#[test]
fn exclude_and_include_instances_filter_every_query() {
    check(
        r#"
        local function near(a, b) return math.abs(a - b) < 1e-3 end
        local function Part(Name, Parent, Y)
            local Made = Instance.new("Part")
            Made.Name, Made.Anchored, Made.Size, Made.CFrame = Name, true, Vector3.new(10, 1, 10), CFrame.new(0, Y, 0)
            Made.Parent = Parent
            return Made
        end
        local Folder = Instance.new("Folder")
        Folder.Name = "Folder"
        Folder.Parent = workspace
        local Tree = Instance.new("Model")
        Tree.Name = "Tree"
        Tree.Parent = Folder
        local Leaf, Trunk, Ground = Part("Leaf", Tree, 9), Part("Trunk", Folder, 6), Part("Ground", workspace, 3)
        local function Seen(Params)
            local Names = {}
            for _, Found in workspace:GetPartBoundsInBox(CFrame.new(0, 6, 0), Vector3.new(4, 10, 4), Params) do
                table.insert(Names, Found.Name)
            end
            table.sort(Names)
            return table.concat(Names, ",")
        end

        local O = OverlapParams.new()
        assert(O.ExcludeInstances == nil and O.IncludeInstances == nil, "nil until assigned")
        assert(Seen(O) == "Ground,Leaf,Trunk", Seen(O))
        -- the announcement's "whitelist with exceptions": Folder's parts but Tree's
        O.IncludeInstances = {Folder}
        O.ExcludeInstances = {Tree}
        assert(Seen(O) == "Trunk", Seen(O))
        -- and "blacklist with exceptions": Folder left out, Tree inside it kept
        O.IncludeInstances = {Tree}
        O.ExcludeInstances = {Folder}
        assert(Seen(O) == "Leaf", Seen(O))
        -- the nearest listed ancestor decides (workspace out, a model in it in: the thread's example)
        O.ExcludeInstances = {workspace}
        assert(Seen(O) == "Leaf", Seen(O))
        -- listed in both: left out
        O.IncludeInstances = {Folder}
        O.ExcludeInstances = {Folder}
        assert(Seen(O) == "", Seen(O))
        -- {} includes nothing, nil everything; {} excludes nothing
        O.ExcludeInstances = nil
        O.IncludeInstances = {}
        assert(Seen(O) == "", Seen(O))
        O.IncludeInstances = nil
        O.ExcludeInstances = {Folder}
        assert(Seen(O) == "Ground", Seen(O))
        O.ExcludeInstances = {}
        assert(Seen(O) == "Ground,Leaf,Trunk", Seen(O))
        -- a part listed itself
        O.IncludeInstances = {workspace}
        O.ExcludeInstances = {Leaf}
        assert(Seen(O) == "Ground,Trunk", Seen(O))
        -- parts that arrive later under a listed instance follow it
        O.IncludeInstances = nil
        O.ExcludeInstances = {Tree}
        local Sprout = Part("Sprout", Tree, 7.5)
        assert(Seen(O) == "Ground,Trunk", Seen(O))
        Sprout.Parent = Folder
        assert(Seen(O) == "Ground,Sprout,Trunk", Seen(O))
        Sprout:Destroy()

        -- with FilterDescendantsInstances too, a part passes both filters
        O.ExcludeInstances = nil
        O.IncludeInstances = {Folder}
        O.FilterDescendantsInstances = {Trunk}
        assert(Seen(O) == "Leaf", Seen(O))
        O.FilterType = Enum.RaycastFilterType.Include
        O.FilterDescendantsInstances = {Folder}
        O.IncludeInstances = nil
        O.ExcludeInstances = {Tree}
        assert(Seen(O) == "Trunk", Seen(O))
        O.FilterType = Enum.RaycastFilterType.Exclude
        O.FilterDescendantsInstances = {}

        -- a table assigned is copied, and one read back is a copy
        local List = {Folder}
        O.ExcludeInstances = List
        table.insert(List, Ground)
        assert(Seen(O) == "Ground", "a table changed after it was assigned changed the filter")
        local Read = O.ExcludeInstances
        assert(#Read == 1 and Read[1] == Folder and Read ~= List and Read ~= O.ExcludeInstances, "a read gives a copy")
        table.insert(Read, Ground)
        assert(Seen(O) == "Ground", "a table read back changed the filter")
        O.ExcludeInstances = List
        assert(Seen(O) == "", "assigning the table again")
        O.ExcludeInstances = nil
        local Legacy = {Leaf}
        O.FilterDescendantsInstances = Legacy
        table.insert(Legacy, Trunk)
        assert(Seen(O) == "Ground,Trunk", "FilterDescendantsInstances is copied as well: " .. Seen(O))
        O.FilterDescendantsInstances = {}
        O.MaxParts = 1
        assert(#workspace:GetPartBoundsInBox(CFrame.new(0, 6, 0), Vector3.new(4, 10, 4), O) == 1, "MaxParts")
        O.MaxParts = 0

        -- the casts
        local Down = Vector3.new(0, -20, 0)
        local R = RaycastParams.new()
        R.ExcludeInstances = {Tree}
        assert(workspace:Raycast(Vector3.new(0, 15, 0), Down, R).Instance == Trunk, "Raycast")
        assert(workspace:Blockcast(CFrame.new(0, 15, 0), Vector3.new(1, 1, 1), Down, R).Instance == Trunk, "Blockcast")
        assert(workspace:Spherecast(Vector3.new(0, 15, 0), 0.5, Down, R).Instance == Trunk, "Spherecast")
        R.IncludeInstances = {Tree}
        R.ExcludeInstances = nil
        assert(workspace:Raycast(Vector3.new(0, 15, 0), Down, R).Instance == Leaf, "Raycast, included")
        R.IncludeInstances = {}
        assert(workspace:Raycast(Vector3.new(0, 15, 0), Down, R) == nil, "Raycast, nothing included")
        R.IncludeInstances = nil
        R.ExcludeInstances = {Tree}
        local Ball = Instance.new("Part")
        Ball.Shape = Enum.PartType.Ball
        Ball.Anchored, Ball.Size, Ball.CFrame = true, Vector3.new(1, 1, 1), CFrame.new(30, 15, 0)
        Ball.Parent = workspace
        local Hit = workspace:Shapecast(Ball, Down, R, CFrame.new(0, 15, 0))
        assert(Hit and Hit.Instance == Trunk and near(Hit.Distance, 8), "Shapecast")
        local Queried = Instance.new("Part")
        Queried.Anchored, Queried.Size, Queried.CFrame = true, Vector3.new(2, 10, 2), CFrame.new(0, 6, 0)
        Queried.Parent = workspace
        O.ExcludeInstances = {Folder}
        local Inside = workspace:GetPartsInPart(Queried, O)
        assert(#Inside == 1 and Inside[1] == Ground, "GetPartsInPart")
        O.ExcludeInstances = {Folder, Queried}
        local Near = workspace:GetPartBoundsInRadius(Vector3.new(0, 6, 0), 4, O)
        assert(#Near == 1 and Near[1] == Ground, "GetPartBoundsInRadius")
        "#,
    );
}

/// WorldRoot:Shapecast(part, direction, params?, cframeOverride?, sizeOverride?) (Roblox's
/// reference): the part's shape cast from where cframeOverride puts it and at sizeOverride's size,
/// else the part's own; the part itself is never hit, what it holds can be.
#[test]
fn shapecast_takes_a_cframe_and_a_size_override() {
    check(
        r#"
        local function near(a, b) return math.abs(a - b) < 1e-3 end
        local Floor = Instance.new("Part")
        Floor.Anchored, Floor.Size, Floor.CFrame = true, Vector3.new(50, 1, 50), CFrame.new(0, -0.5, 0)
        Floor.Parent = workspace
        local Down = Vector3.new(0, -20, 0)
        local Ball = Instance.new("Part")
        Ball.Shape = Enum.PartType.Ball
        Ball.Anchored, Ball.Size, Ball.CFrame = true, Vector3.new(1, 1, 1), CFrame.new(100, 10, 0)
        Ball.Parent = workspace
        assert(workspace:Shapecast(Ball, Down) == nil, "from where it is it reaches nothing")
        local Hit = workspace:Shapecast(Ball, Down, nil, CFrame.new(0, 10, 0))
        assert(Hit and Hit.Instance == Floor and near(Hit.Distance, 9.5), Hit and Hit.Distance)
        Hit = workspace:Shapecast(Ball, Down, nil, CFrame.new(0, 10, 0), Vector3.new(4, 4, 4))
        assert(Hit and Hit.Instance == Floor and near(Hit.Distance, 8), Hit and Hit.Distance)
        local Box = Instance.new("Part")
        Box.Anchored, Box.Size, Box.CFrame = true, Vector3.new(2, 2, 2), CFrame.new(100, 10, 20)
        Box.Parent = workspace
        Hit = workspace:Shapecast(Box, Down, nil, CFrame.new(0, 10, 0))
        assert(Hit and Hit.Instance == Floor and near(Hit.Distance, 9), Hit and Hit.Distance)
        Hit = workspace:Shapecast(Box, Down, nil, CFrame.new(0, 10, 0), Vector3.new(1, 6, 1))
        assert(Hit and Hit.Instance == Floor and near(Hit.Distance, 7), Hit and Hit.Distance)
        assert(Box.CFrame == CFrame.new(100, 10, 20) and Box.Size == Vector3.new(2, 2, 2), "the part stays as it was")
        -- a part the cast part holds is hit; the cast part never is
        local Held = Instance.new("Part")
        Held.Anchored, Held.Size, Held.CFrame = true, Vector3.new(4, 1, 4), CFrame.new(0, 4, 0)
        Held.Parent = Box
        Hit = workspace:Shapecast(Box, Down, nil, CFrame.new(0, 10, 0))
        assert(Hit and Hit.Instance == Held and near(Hit.Distance, 4.5), Hit and Hit.Instance.Name)
        Hit = workspace:Shapecast(Box, Vector3.new(0, 0, 40), nil, CFrame.new(100, 10, 0))
        assert(Hit == nil, "the cast part itself")
        "#,
    );
}

/// Bone.TransformedCFrame is CFrame * Transform; TransformedWorldCFrame chains it through parent bones
/// to the part; WorldCFrame leaves out the bone's own Transform but carries its parent bones' (measured
/// in Studio 2026-10-16), and an Attachment under a bone hangs from its TransformedWorldCFrame.
#[test]
fn a_bone_reports_its_transformed_cframes() {
    check(
        r#"
        local function near(a, b) return (a.Position - b.Position).Magnitude < 1e-4 and a.XVector:Dot(b.XVector) > 0.99999 and a.YVector:Dot(b.YVector) > 0.99999 end
        local Part = Instance.new("Part")
        Part.CFrame = CFrame.new(10, 0, 0) * CFrame.Angles(0, math.rad(90), 0)
        Part.Parent = workspace
        local Root = Instance.new("Bone")
        Root.CFrame = CFrame.new(0, 1, 0)
        Root.Parent = Part
        local Tip = Instance.new("Bone")
        Tip.CFrame = CFrame.new(0, 2, 0)
        Tip.Parent = Root
        assert(near(Tip.TransformedWorldCFrame, Part.CFrame * CFrame.new(0, 3, 0)), "at rest")
        local Bend = CFrame.Angles(0, 0, math.rad(90))
        Root.Transform = Bend
        Tip.Transform = CFrame.new(0, 0.5, 0)
        assert(near(Root.TransformedCFrame, Root.CFrame * Bend), "TransformedCFrame")
        assert(near(Root.TransformedWorldCFrame, Part.CFrame * Root.CFrame * Bend), "the root's TransformedWorldCFrame")
        assert(near(Tip.TransformedWorldCFrame, Part.CFrame * Root.CFrame * Bend * Tip.CFrame * Tip.Transform), "the tip carries its parent's Transform")
        assert(near(Root.WorldCFrame, Part.CFrame * Root.CFrame), "a bone's WorldCFrame leaves out its own Transform")
        assert(near(Tip.WorldCFrame, Part.CFrame * Root.CFrame * Bend * Tip.CFrame), "and carries its parent bone's")
        "#,
    );
}

/// Studio's own numbers (a Part at (10, 0, 0), bones at (0, 5, 0) and (0, 2, 0) under it, an
/// Attachment 3 studs along Z on the root and one 1 stud along X on the tip, the root's Transform 7
/// along Z and 90 degrees about Z, the tip's 11 along Y and 90 degrees about X): attachments follow
/// their bone's animation.
#[test]
fn attachments_follow_the_bone_they_hang_from() {
    check(
        r#"
        local function at(c, x, y, z) return (c.Position - Vector3.new(x, y, z)).Magnitude < 1e-3 end
        local Part = Instance.new("Part")
        Part.CFrame = CFrame.new(10, 0, 0)
        Part.Parent = workspace
        local Root = Instance.new("Bone")
        Root.CFrame = CFrame.new(0, 5, 0)
        Root.Parent = Part
        local Tip = Instance.new("Bone")
        Tip.CFrame = CFrame.new(0, 2, 0)
        Tip.Parent = Root
        local OnTip = Instance.new("Attachment")
        OnTip.CFrame = CFrame.new(1, 0, 0)
        OnTip.Parent = Tip
        local OnRoot = Instance.new("Attachment")
        OnRoot.CFrame = CFrame.new(0, 0, 3)
        OnRoot.Parent = Root
        Root.Transform = CFrame.new(0, 0, 7) * CFrame.Angles(0, 0, math.rad(90))
        Tip.Transform = CFrame.new(0, 11, 0) * CFrame.Angles(math.rad(90), 0, 0)
        assert(at(Root.WorldCFrame, 10, 5, 0), "root WorldCFrame")
        assert(at(Root.TransformedWorldCFrame, 10, 5, 7), "root TransformedWorldCFrame")
        assert(at(Tip.WorldCFrame, 8, 5, 7), "tip WorldCFrame")
        assert(at(Tip.TransformedWorldCFrame, -3, 5, 7), "tip TransformedWorldCFrame")
        assert(at(OnRoot.WorldCFrame, 10, 5, 10), "the root's attachment")
        assert(at(OnTip.WorldCFrame, -3, 6, 7), "the tip's attachment")
        assert(at(CFrame.new(OnTip.WorldPosition), -3, 6, 7), "WorldPosition alike")
        OnTip.WorldPosition = Vector3.new(-3, 6, 9)
        assert(at(OnTip.WorldCFrame, -3, 6, 9), "a world write lands where it was put")
        "#,
    );
}

#[test]
fn shape_casts_and_overlaps() {
    check(
        r#"
        local function near(a, b) return math.abs(a - b) < 1e-3 end
        local Wall = Instance.new("Part")
        Wall.Name = "Wall"
        Wall.Size = Vector3.new(2, 10, 10)
        Wall.CFrame = CFrame.new(10, 0, 0)
        Wall.Parent = workspace
        -- a 2-stud box moving +X touches the wall's face at x = 9 when its centre is at 8
        local Hit = workspace:Blockcast(CFrame.new(0, 0, 0), Vector3.new(2, 2, 2), Vector3.new(20, 0, 0))
        assert(Hit and Hit.Instance == Wall and near(Hit.Distance, 8) and near(Hit.Normal.X, -1), Hit and Hit.Distance)
        assert(near(Hit.Position.X, 9))
        -- a sphere of radius 1 the same
        Hit = workspace:Spherecast(Vector3.new(0, 0, 0), 1, Vector3.new(20, 0, 0))
        assert(Hit and Hit.Instance == Wall and near(Hit.Distance, 8) and near(Hit.Position.X, 9))
        -- a cast that starts overlapping does not hit
        assert(workspace:Blockcast(CFrame.new(9.5, 0, 0), Vector3.new(2, 2, 2), Vector3.new(5, 0, 0)) == nil)
        -- overlap queries
        local Found = workspace:GetPartBoundsInRadius(Vector3.new(7, 0, 0), 2.5)
        assert(#Found == 1 and Found[1] == Wall)
        assert(#workspace:GetPartBoundsInRadius(Vector3.new(5, 0, 0), 2.5) == 0)
        assert(#workspace:GetPartBoundsInBox(CFrame.new(8.5, 0, 0), Vector3.new(1.2, 1, 1)) == 1)
        assert(#workspace:GetPartBoundsInBox(CFrame.new(8, 0, 0), Vector3.new(1, 1, 1)) == 0)
        local Probe = Instance.new("Part")
        Probe.Size = Vector3.new(3, 3, 3)
        Probe.CFrame = CFrame.new(9, 0, 0)
        Probe.Parent = workspace
        local Inside = workspace:GetPartsInPart(Probe)
        assert(#Inside == 1 and Inside[1] == Wall)
        local Params = OverlapParams.new()
        Params.FilterDescendantsInstances = {Wall}
        assert(#workspace:GetPartsInPart(Probe, Params) == 0)
        "#,
    );
}

#[test]
fn camera_projections() {
    check(
        r#"
        local Camera = workspace.CurrentCamera
        Camera.FieldOfView = 90
        Camera.CFrame = CFrame.lookAt(Vector3.new(0, 0, 10), Vector3.zero)
        local Size = Camera.ViewportSize
        -- the point looked at is the viewport's centre, 10 studs deep
        local Centre, Visible = Camera:WorldToViewportPoint(Vector3.zero)
        assert(Visible and math.abs(Centre.X - Size.X / 2) < 1e-3 and math.abs(Centre.Y - Size.Y / 2) < 1e-3 and math.abs(Centre.Z - 10) < 1e-4, tostring(Centre))
        -- at 90 degrees, a point as far up as it is deep is the top edge
        local Top = Camera:WorldToViewportPoint(Vector3.new(0, 10, 0))
        assert(math.abs(Top.Y) < 1e-2, tostring(Top))
        local _, Behind = Camera:WorldToViewportPoint(Vector3.new(0, 0, 20))
        assert(not Behind)
        -- the ray through the centre points at the origin, starting where asked
        local Ray = Camera:ViewportPointToRay(Size.X / 2, Size.Y / 2, 2)
        assert((Ray.Direction - Vector3.new(0, 0, -1)).Magnitude < 1e-4 and (Ray.Origin - Vector3.new(0, 0, 8)).Magnitude < 1e-4, tostring(Ray.Origin))
        local Screen = Camera:ScreenPointToRay(Size.X / 2, Size.Y / 2)
        assert((Screen.Direction - Vector3.new(0, 0, -1)).Magnitude < 1e-4)
        "#,
    );
}

#[test]
fn bulk_moves_and_reported_calls() {
    let (lua, engine) = start();
    let events: Rc<RefCell<Vec<WorldEvent>>> = Rc::default();
    let seen = events.clone();
    engine.hooks.borrow_mut().world_events.push(Box::new(move |_, event| seen.borrow_mut().push(event.clone())));
    run(
        &lua,
        &engine,
        r#"
        local Part = Instance.new("Part")
        workspace:BulkMoveTo({Part}, {CFrame.new(3, 4, 5)}, Enum.BulkMoveMode.FireCFrameChanged)
        assert(Part.Position == Vector3.new(3, 4, 5))
        local Emitter = Instance.new("ParticleEmitter")
        Emitter.Parent = Part
        Emitter:Emit(7)
        Emitter:Emit()
        Emitter:Clear()
        local Trail = Instance.new("Trail")
        Trail:Clear()
        local Sound = Instance.new("Sound")
        Sound:Play()
        assert(Sound.Playing)
        Sound:Stop()
        game:GetService("SoundService"):PlayLocalSound(Sound)
        local Player = Instance.new("AudioPlayer")
        Player:Play()
        assert(Player.IsPlaying)
        Player:Stop()
        assert(not Player.IsPlaying)
        "#,
    );
    let events = events.borrow();
    let kinds: Vec<String> = events
        .iter()
        .map(|event| match event {
            WorldEvent::Emit { count, .. } => format!("emit {count}"),
            WorldEvent::Clear { .. } => "clear".into(),
            WorldEvent::SoundPlayed { local, .. } => format!("play {local}"),
            WorldEvent::SoundStopped { .. } => "stop".into(),
        })
        .collect();
    assert_eq!(kinds, ["emit 7", "emit 16", "clear", "clear", "play false", "stop", "play true", "play false", "stop"]);
}

// Rigid assemblies (joints.rs)

/// Runs `source` in an engine of `mode` for `seconds` of frames; panics unless it finished cleanly.
fn check_running(mode: &str, source: &str, seconds: f64) {
    check_with(Options { mode: mode.into(), ..Options::default() }, source, seconds);
}

/// Runs `source` in an engine of `options` for `seconds` of frames; panics unless it finished cleanly.
fn check_with(options: Options, source: &str, seconds: f64) {
    let engine = Engine::new(options).expect("engine");
    let lua = Lua::new();
    engine_lua::start(&lua, &engine).expect("start");
    let script = {
        let mut state = engine.state.borrow_mut();
        let class = engine.reflection.class_id("ModuleScript").unwrap();
        crate::instance::create_named(&mut state, &engine.reflection, class, "Test")
    };
    let env = engine_lua::modules::environment(&lua, &engine, script).expect("environment");
    let prelude = "local function near(a, b) return (a.Position - b.Position).Magnitude < 1e-3 and a.XVector:Dot(b.XVector) > 0.99999 and a.YVector:Dot(b.YVector) > 0.99999 end\n";
    let function = lua.load(format!("{prelude}{source}")).set_name("=Test").set_environment(env).into_function().expect("compile");
    let call = engine_lua::globals::call(&lua, &engine, function, LuaMultiValue::new()).expect("call");
    crate::scheduler::run_until(&lua, &engine, seconds).expect("run");
    match call.outcome().unwrap() {
        CallOutcome::Failed(message, trace) => panic!("test script failed: {message}\n{}", trace.unwrap_or_default()),
        CallOutcome::Pending => panic!("test script did not finish"),
        CallOutcome::Finished(_) => {}
    }
}

#[test]
fn a_weld_carries_its_part() {
    check_running(
        "play",
        r#"
        local A = Instance.new("Part")
        A.Anchored = true
        A.CFrame = CFrame.new(0, 5, 0)
        A.Parent = workspace
        local B = Instance.new("Part")
        B.CFrame = CFrame.new(50, 0, 0)
        B.Parent = workspace
        local Weld = Instance.new("Weld")
        Weld.Part0, Weld.Part1 = A, B
        Weld.C0 = CFrame.new(0, 2, 0)
        Weld.C1 = CFrame.Angles(0, math.rad(90), 0)
        -- outside the Workspace a joint links nothing
        assert(near(B.CFrame, CFrame.new(50, 0, 0)), "inactive weld moved its part")
        Weld.Parent = A
        -- active: the root (A, anchored) stays and B goes where the weld puts it
        assert(near(A.CFrame, CFrame.new(0, 5, 0)), "the root moved")
        assert(near(B.CFrame, A.CFrame * Weld.C0 * Weld.C1:Inverse()), "B did not join A")
        -- moving A carries B, without Changed events for B
        local Fired = 0
        B:GetPropertyChangedSignal("CFrame"):Connect(function() Fired += 1 end)
        A.CFrame = CFrame.new(10, 0, 0) * CFrame.Angles(0, math.rad(45), 0)
        assert(near(B.CFrame, A.CFrame * Weld.C0 * Weld.C1:Inverse()), "B did not follow A")
        task.wait()
        assert(Fired == 0, "a carried part fired CFrame changes")
        -- moving B carries the whole assembly, its anchored root too
        B.CFrame = CFrame.new(-20, 3, 0)
        assert(near(B.CFrame, CFrame.new(-20, 3, 0)), "B is not where it was set")
        assert(near(A.CFrame, B.CFrame * Weld.C1 * Weld.C0:Inverse()), "A did not follow B")
        -- C0 changes: the root stays, the rest moves
        local Root = A.CFrame
        Weld.C0 = CFrame.new(0, 0, -4)
        assert(near(A.CFrame, Root), "the root moved for C0")
        assert(near(B.CFrame, Root * Weld.C0 * Weld.C1:Inverse()), "B did not take the new C0")
        -- Position moves B alone: the weld's C1 is rewritten to keep it there
        B.Position = Vector3.new(0, 30, 0)
        assert(near(A.CFrame, Root), "Position moved the root")
        assert(B.Position == Vector3.new(0, 30, 0), "Position did not move B")
        assert(near(B.CFrame, A.CFrame * Weld.C0 * Weld.C1:Inverse()), "C1 does not hold B where it is")
        A.CFrame = A.CFrame + Vector3.new(0, 1, 0)
        assert(near(B.CFrame, CFrame.new(0, 31, 0) * B.CFrame.Rotation), "B did not keep its new place")
        -- disabled or unparented: no link
        Weld.Enabled = false
        local Rest = B.CFrame
        A.CFrame = CFrame.new(100, 0, 0)
        assert(near(B.CFrame, Rest), "a disabled weld carried its part")
        Weld.Enabled = true
        assert(near(B.CFrame, A.CFrame * Weld.C0 * Weld.C1:Inverse()), "re-enabling did not join B")
        Weld.Parent = nil
        A.CFrame = CFrame.new(0, 0, 0)
        assert(near(B.CFrame, CFrame.new(100, 0, 0) * Weld.C0 * Weld.C1:Inverse()), "an unparented weld carried its part")
        "#,
        0.2,
    );
}

#[test]
fn a_motor6d_applies_its_transform_after_presimulation() {
    check_running(
        "play",
        r#"
        local RunService = game:GetService("RunService")
        local Torso = Instance.new("Part")
        Torso.Name = "Torso"
        Torso.Anchored = true
        Torso.CFrame = CFrame.new(0, 3, 0)
        Torso.Parent = workspace
        local Arm = Instance.new("Part")
        Arm.Size = Vector3.new(1, 2, 1)
        Arm.Parent = workspace
        local Motor = Instance.new("Motor6D")
        Motor.Part0, Motor.Part1 = Torso, Arm
        Motor.C0 = CFrame.new(1.5, 0.5, 0)
        Motor.C1 = CFrame.new(0, 0.5, 0)
        Motor.Parent = Torso
        local Rest = Torso.CFrame * Motor.C0 * Motor.C1:Inverse()
        assert(near(Arm.CFrame, Rest), "the arm did not join the torso")
        task.wait()
        local Pose = CFrame.Angles(math.rad(90), 0, 0)
        local Posed = Torso.CFrame * Motor.C0 * Pose * Motor.C1:Inverse()
        Motor.Transform = Pose
        -- Transform waits for the simulation step (C0 and C1 apply at once)
        assert(near(Arm.CFrame, Rest), "Transform applied before the simulation step")
        local AtPre, AtPost
        local Pre, Post
        Pre = RunService.PreSimulation:Connect(function() AtPre = Arm.CFrame Pre:Disconnect() end)
        Post = RunService.PostSimulation:Connect(function() AtPost = Arm.CFrame Post:Disconnect() end)
        task.wait()
        assert(AtPre and near(AtPre, Rest), "PreSimulation saw the new pose")
        assert(AtPost and near(AtPost, Posed), "PostSimulation did not see the pose")
        assert(near(Arm.CFrame, Posed), "the pose did not stay")
        -- the torso moving carries the posed arm
        Torso.CFrame = CFrame.new(5, 3, 5)
        assert(near(Arm.CFrame, Torso.CFrame * Motor.C0 * Pose * Motor.C1:Inverse()), "the posed arm did not follow")
        "#,
        0.3,
    );
}

#[test]
fn a_motor6d_transform_applies_in_edit_mode_too() {
    check_running(
        "edit",
        r#"
        local Root = Instance.new("Part")
        Root.Anchored = true
        Root.Parent = workspace
        local Limb = Instance.new("Part")
        Limb.Parent = workspace
        local Motor = Instance.new("Motor6D")
        Motor.Part0, Motor.Part1 = Root, Limb
        Motor.C0 = CFrame.new(0, 2, 0)
        Motor.Parent = Root
        Motor.Transform = CFrame.Angles(0, 0, math.rad(30))
        task.wait()
        assert(near(Limb.CFrame, Root.CFrame * Motor.C0 * Motor.Transform), "edit mode did not apply Transform")
        "#,
        0.2,
    );
}

#[test]
fn a_weld_constraint_keeps_where_it_linked_its_parts() {
    check_running(
        "play",
        r#"
        local A = Instance.new("Part")
        A.Anchored = true
        A.Parent = workspace
        local B = Instance.new("Part")
        B.CFrame = CFrame.new(3, 1, 0) * CFrame.Angles(0, math.rad(30), 0)
        B.Parent = workspace
        local Offset = A.CFrame:ToObjectSpace(B.CFrame)
        local Weld = Instance.new("WeldConstraint")
        Weld.Part0, Weld.Part1 = A, B
        Weld.Parent = A
        -- linking moves nothing; then they move together
        assert(near(B.CFrame, CFrame.new(3, 1, 0) * CFrame.Angles(0, math.rad(30), 0)), "linking moved B")
        A.CFrame = CFrame.new(0, 10, 0) * CFrame.Angles(math.rad(20), 0, 0)
        assert(near(B.CFrame, A.CFrame * Offset), "B did not follow A")
        B.CFrame = CFrame.new(7, 7, 7)
        assert(near(A.CFrame, B.CFrame * Offset:Inverse()), "A did not follow B")
        -- Position moves a part alone and the weld takes the new offset
        B.Position = B.Position + Vector3.new(0, 0, 5)
        local Moved = A.CFrame:ToObjectSpace(B.CFrame)
        A.CFrame = CFrame.new(-4, 0, 0)
        assert(near(B.CFrame, A.CFrame * Moved), "B did not keep its new offset")
        -- Part1 set again: links where B is now
        Weld.Part1 = nil
        B.CFrame = CFrame.new(0, 50, 0)
        Weld.Part1 = B
        A.CFrame = CFrame.new(-4, 1, 0)
        assert(near(B.CFrame, CFrame.new(0, 51, 0)), "relinking did not keep B where it was")

        -- a clone keeps its welds' offsets: moved by its root out of the Workspace, it rejoins it in
        local Model = Instance.new("Model")
        local Root = Instance.new("Part")
        Root.Name = "Root"
        Root.Anchored = true
        Root.Parent = Model
        local Handle = Instance.new("Part")
        Handle.Name = "Handle"
        Handle.CFrame = CFrame.new(0, 4, 0)
        Handle.Parent = Model
        local Link = Instance.new("WeldConstraint")
        Link.Part0, Link.Part1 = Root, Handle
        Link.Parent = Handle
        local Copy = Model:Clone()
        Copy.Root.CFrame = CFrame.new(20, 0, 0)
        assert(near(Copy.Handle.CFrame, CFrame.new(0, 4, 0)), "an inactive weld carried its part")
        Copy.Parent = workspace
        assert(near(Copy.Handle.CFrame, CFrame.new(20, 4, 0)), "the clone's handle did not rejoin its root")
        "#,
        0.2,
    );
}

#[test]
fn a_rig_follows_its_humanoid_root_part() {
    check_running(
        "play",
        r#"
        -- An R6-like rig, nothing anchored: HumanoidRootPart is the root (its 10x size weight)
        local Rig = Instance.new("Model")
        local function Part(Name, Size, At)
            local Made = Instance.new("Part")
            Made.Name, Made.Size, Made.CFrame = Name, Size, At
            Made.Parent = Rig
            return Made
        end
        local Root = Part("HumanoidRootPart", Vector3.new(2, 2, 1), CFrame.new(0, 3, 0))
        local Torso = Part("Torso", Vector3.new(2, 2, 1), CFrame.new(0, 3, 0))
        local Head = Part("Head", Vector3.new(2, 1, 1), CFrame.new(0, 4.5, 0))
        local Arm = Part("Right Arm", Vector3.new(1, 2, 1), CFrame.new(1.5, 3, 0))
        local Hat = Part("Handle", Vector3.new(3, 1, 3), CFrame.new(0, 5.5, 0))
        local function Motor(Name, Part0, Part1, C0, C1)
            local Made = Instance.new("Motor6D")
            Made.Name, Made.Part0, Made.Part1, Made.C0, Made.C1 = Name, Part0, Part1, C0, C1
            Made.Parent = Part0
            return Made
        end
        local RootJoint = Motor("RootJoint", Root, Torso, CFrame.identity, CFrame.identity)
        local Neck = Motor("Neck", Torso, Head, CFrame.new(0, 1, 0), CFrame.new(0, -0.5, 0))
        Motor("Right Shoulder", Torso, Arm, CFrame.new(1, 0.5, 0), CFrame.new(-0.5, 0.5, 0))
        local HatWeld = Instance.new("WeldConstraint")
        HatWeld.Part0, HatWeld.Part1 = Head, Hat
        HatWeld.Parent = Hat
        Rig.PrimaryPart = Root
        Rig.Parent = workspace
        -- a consistent rig does not move when it links up
        assert(near(Head.CFrame, CFrame.new(0, 4.5, 0)) and near(Arm.CFrame, CFrame.new(1.5, 3, 0)), "the rig moved when it linked")

        -- the client moves the root (CFrame, BulkMoveTo): the body comes along
        local Goal = CFrame.new(100, 3, 50) * CFrame.Angles(0, math.rad(120), 0)
        Root.CFrame = Goal
        assert(near(Torso.CFrame, Goal), "the torso stayed")
        assert(near(Head.CFrame, Goal * CFrame.new(0, 1.5, 0)), "the head stayed")
        assert(near(Arm.CFrame, Goal * CFrame.new(1.5, 0, 0)), "the arm stayed")
        assert(near(Hat.CFrame, Goal * CFrame.new(0, 2.5, 0)), "the hat stayed")
        workspace:BulkMoveTo({Root}, {CFrame.new(-30, 3, 0)}, Enum.BulkMoveMode.FireCFrameChanged)
        assert(near(Head.CFrame, CFrame.new(-30, 4.5, 0)) and near(Hat.CFrame, CFrame.new(-30, 5.5, 0)), "BulkMoveTo left the body")

        -- moving a limb moves everything (it is one assembly)
        Arm.CFrame = Arm.CFrame + Vector3.new(0, 10, 0)
        assert(near(Root.CFrame, CFrame.new(-30, 13, 0)), "the root did not follow the arm")

        -- Model:PivotTo moves the rig as one
        Rig:PivotTo(CFrame.new(7, 3, 7))
        assert(near(Root.CFrame, CFrame.new(7, 3, 7)) and near(Head.CFrame, CFrame.new(7, 4.5, 7)), "PivotTo split the rig")

        -- a joint's C0 moves what hangs below it, never the root
        Neck.C0 = CFrame.new(0, 2, 0)
        assert(near(Root.CFrame, CFrame.new(7, 3, 7)), "the root moved for the neck")
        assert(near(Head.CFrame, CFrame.new(7, 5.5, 7)) and near(Hat.CFrame, CFrame.new(7, 6.5, 7)), "the head did not take the neck's C0")

        -- RootPriority picks another root: the RootJoint's C0 now moves the HumanoidRootPart
        Torso.RootPriority = 1
        RootJoint.C0 = CFrame.new(0, -1, 0)
        assert(near(Torso.CFrame, CFrame.new(7, 3, 7)), "the torso (now the root) moved")
        assert(near(Root.CFrame, CFrame.new(7, 4, 7)), "the old root did not move")

        -- ScaleTo scales the joints with the parts, so the rig stays joined
        Torso.RootPriority = 0
        RootJoint.C0 = CFrame.identity
        Rig:ScaleTo(2)
        assert(near(Head.CFrame, Root.CFrame * CFrame.new(0, 5, 0)), "ScaleTo broke the neck")
        assert(near(Hat.CFrame, Root.CFrame * CFrame.new(0, 7, 0)), "ScaleTo broke the hat's weld")
        Root.CFrame = CFrame.new(0, 6, 0)
        assert(near(Hat.CFrame, CFrame.new(0, 13, 0)), "the scaled hat did not follow")
        "#,
        0.2,
    );
}

#[test]
fn an_anchored_root_stays_and_anchored_parts_do_not_link() {
    check_running(
        "play",
        r#"
        -- a small anchored part is the root over a big unanchored one
        local Base = Instance.new("Part")
        Base.Size = Vector3.new(1, 1, 1)
        Base.Anchored = true
        Base.CFrame = CFrame.new(0, 0, 0)
        Base.Parent = workspace
        local Slab = Instance.new("Part")
        Slab.Size = Vector3.new(40, 1, 40)
        Slab.CFrame = CFrame.new(0, 5, 0)
        Slab.Parent = workspace
        local Weld = Instance.new("Weld")
        Weld.Part0, Weld.Part1 = Slab, Base
        Weld.C0 = CFrame.new(0, -5, 0)
        Weld.Parent = Slab
        Weld.C0 = CFrame.new(0, -8, 0)
        assert(near(Base.CFrame, CFrame.new(0, 0, 0)), "the anchored root moved")
        assert(near(Slab.CFrame, CFrame.new(0, 8, 0)), "the slab did not move")
        -- two anchored parts: the weld between them does nothing
        Slab.Anchored = true
        Base.CFrame = CFrame.new(0, -3, 0)
        assert(near(Slab.CFrame, CFrame.new(0, 8, 0)), "an anchored part followed another")
        -- unanchored again: the weld links them, the slab goes where it puts it from its anchored root
        Slab.Anchored = false
        assert(near(Base.CFrame, CFrame.new(0, -3, 0)), "the anchored root moved when the weld linked")
        assert(near(Slab.CFrame, CFrame.new(0, 5, 0)), "the slab did not rejoin its root")
        Base.CFrame = CFrame.new(0, 0, 0)
        assert(near(Slab.CFrame, CFrame.new(0, 8, 0)), "the slab did not follow its root")
        "#,
        0.2,
    );
}

/// A pose landing while the rig moves leaves its anchored root where the moves put it: follower
/// rigs (anchored HumanoidRootPart, Model:PivotTo each frame) whose idle pose was set
/// a defer after they were built lay on their side.
#[test]
fn an_anchored_root_stays_when_a_pose_lands_while_it_moves() {
    check_running(
        "play",
        r#"
        local RunService = game:GetService("RunService")
        task.wait(0.1)
        local Rig = Instance.new("Model")
        local function Part(Name, Size, At)
            local Made = Instance.new("Part")
            Made.Name, Made.Size, Made.CFrame = Name, Size, At
            Made.Anchored = Name == "HumanoidRootPart"
            Made.Massless = Name ~= "HumanoidRootPart"
            Made.Parent = Rig
            return Made
        end
        local Root = Part("HumanoidRootPart", Vector3.new(2, 2, 1), CFrame.new(0, 3, 0))
        local Torso = Part("Torso", Vector3.new(2, 2, 1), CFrame.new(0, 3, 0))
        local Head = Part("Head", Vector3.new(2, 1, 1), CFrame.new(0, 4.5, 0))
        local Arm = Part("Right Arm", Vector3.new(1, 2, 1), CFrame.new(1.5, 3, 0))
        local function Motor(Name, Part0, Part1, C0, C1)
            local Made = Instance.new("Motor6D")
            Made.Name, Made.Part0, Made.Part1, Made.C0, Made.C1 = Name, Part0, Part1, C0, C1
            Made.Parent = Part0
            return Made
        end
        local RootJoint = Motor("RootJoint", Root, Torso, CFrame.identity, CFrame.identity)
        local Neck = Motor("Neck", Torso, Head, CFrame.new(0, 1, 0), CFrame.new(0, -0.5, 0))
        Motor("Right Shoulder", Torso, Arm, CFrame.new(1, 0.5, 0), CFrame.new(-0.5, 0.5, 0))
        Rig.PrimaryPart = Root
        Rig.Parent = workspace

        -- the pose lands a defer later, and the rig is scaled about its root in the same frame: the
        -- scale writes the root where it is (it counts as written) and moves the rest
        local Lean = CFrame.Angles(0, 0, math.rad(80))
        local Done = false
        task.defer(function()
            RootJoint.Transform = Lean
            Neck.Transform = CFrame.Angles(math.rad(40), 0, 0)
            Rig:ScaleTo(2)
            Done = true
        end)
        task.wait()
        assert(Done, "the defer did not run")
        assert(near(Root.CFrame, CFrame.new(0, 3, 0)), "the pose moved the anchored root")
        assert(near(Torso.CFrame, Root.CFrame * Lean), "the pose did not lean the torso")

        -- a limb's CFrame written while a Transform waits moves the root as the limb moved, never
        -- swung about the limb by the waiting Transform
        RootJoint.Transform = CFrame.Angles(0, math.rad(90), 0)
        Head.CFrame = Head.CFrame + Vector3.new(5, 0, 0)
        -- (within the float noise of the posed head's rotation: no swing is 1e-4 of a radian)
        local Upright = Root.CFrame.UpVector:Dot(Vector3.yAxis) > 0.9999 and Root.CFrame.LookVector:Dot(-Vector3.zAxis) > 0.9999
        assert(Upright and (Root.Position - Vector3.new(5, 3, 0)).Magnitude < 1e-3, "the waiting Transform swung the root")
        Root.CFrame = CFrame.new(5, 3, 0)

        -- moved each frame by Model:PivotTo, it stays upright
        local Frames = 0
        local Connection = RunService.Heartbeat:Connect(function()
            Frames += 1
            Rig:PivotTo(CFrame.new(5, 3, -0.25 * Frames))
            assert(near(Root.CFrame, CFrame.new(5, 3, -0.25 * Frames)), "PivotTo tilted the anchored root")
        end)
        task.wait(0.1)
        Connection:Disconnect()
        assert(Frames > 3, "the rig did not move")
        assert(near(Torso.CFrame, Root.CFrame * CFrame.Angles(0, math.rad(90), 0)), "the torso lost its pose")
        "#,
        0.5,
    );
}

// Free fall (gravity.rs)

#[test]
fn a_free_assembly_falls_under_gravity() {
    check_running(
        "play",
        r#"
        local function part(name, anchored, collide, y)
            local P = Instance.new("Part")
            P.Name = name
            P.Anchored = anchored
            P.CanCollide = collide
            P.CFrame = CFrame.new(0, y, 0)
            return P
        end
        local Free = part("Free", false, false, 500)
        local Welded = part("Welded", false, false, 510)
        local Weld = Instance.new("WeldConstraint")
        Weld.Part0, Weld.Part1 = Free, Welded
        Weld.Parent = Free
        local Colliding = part("Colliding", false, true, 500)
        local Anchored = part("Anchored", true, false, 500)
        local Held = part("Held", false, false, 500)
        local HeldTo = part("HeldTo", true, false, 600)
        local Joint = Instance.new("WeldConstraint")
        Joint.Part0, Joint.Part1 = Held, HeldTo
        Joint.Parent = Held
        local Elsewhere = part("Elsewhere", false, false, 500)
        local World = Instance.new("WorldModel")
        World.Parent = workspace
        Elsewhere.Parent = World
        for _, P in { Free, Welded, Colliding, Anchored, Held, HeldTo } do
            P.Parent = workspace
        end
        local Fired = 0
        Free:GetPropertyChangedSignal("CFrame"):Connect(function() Fired += 1 end)
        local Start = os.clock()
        task.wait(1)
        local T = os.clock() - Start
        local Drop = 500 - Free.Position.Y
        -- ½ g t² within a frame's fall (Studio: 37.5 studs at 0.62 s, 99.8 at 0.99)
        assert(math.abs(Drop - 0.5 * workspace.Gravity * T * T) < 4, `fell {Drop} in {T}`)
        assert(math.abs((510 - Welded.Position.Y) - Drop) < 1e-2, "the welded part did not fall with it")
        assert(Free.AssemblyLinearVelocity.Y < -150 or Free.Velocity.Y < -150, "no velocity")
        assert(Fired == 0, "physics fired CFrame changes")
        for _, P in { Colliding, Anchored, Held, Elsewhere } do
            assert(P.Position.Y == 500, P.Name .. " moved")
        end
        Anchored.Anchored = false
        task.wait(0.1)
        assert(Anchored.Position.Y < 500, "unanchoring did not free it")
        "#,
        1.5,
    );
}

#[test]
fn nothing_falls_in_edit_mode() {
    check_running(
        "edit",
        r#"
        local P = Instance.new("Part")
        P.Anchored = false
        P.CanCollide = false
        P.CFrame = CFrame.new(0, 500, 0)
        P.Parent = workspace
        task.wait(0.5)
        assert(P.Position.Y == 500, "a part fell in edit mode")
        "#,
        1.0,
    );
}

#[test]
fn a_streaming_place_simulates_near_its_foci() {
    check_with(
        Options { mode: "play".into(), streaming_target_radius: Some(400.0), ..Options::default() },
        r#"
        assert(workspace.StreamingEnabled, "the place does not stream")
        local function part(at)
            local P = Instance.new("Part")
            P.Anchored = false
            P.CanCollide = false
            P.CFrame = CFrame.new(at)
            P.Parent = workspace
            return P
        end
        -- no focus: nothing is held
        local Free = part(Vector3.new(5000, 500, 0))
        task.wait(0.2)
        assert(Free.Position.Y < 500, "held without a focus")
        local Focus = Instance.new("Part")
        Focus.Anchored = true
        Focus.CFrame = CFrame.new(0, 0, 0)
        Focus.Parent = workspace
        game:GetService("Players").LocalPlayer.ReplicationFocus = Focus
        local Near = part(Vector3.new(300, 100, 0))
        local Far = part(Vector3.new(2048, 2048, 2048))
        task.wait(0.3)
        assert(Near.Position.Y < 100, "a part within the radius did not fall")
        assert(Far.Position.Y == 2048, "a part beyond the radius fell")
        -- the focus moves onto it: it falls from rest
        Focus.CFrame = CFrame.new(2048, 2048, 2048)
        task.wait(0.5)
        local Drop = 2048 - Far.Position.Y
        assert(Drop > 15 and Drop < 30, `fell {Drop} in 0.5 s from rest`)
        "#,
        1.5,
    );
}

/// An assembly starting about a focus is held to its reach (StreamingTargetRadius cut to a whole 64,
/// 32 less for an added focus) and then stands still keeping its velocity; one clear of every focus
/// falls on (Studio, target radius 400: a 400-stud cube about an added focus fell 5.9 studs in 0.21 s).
#[test]
fn a_streaming_place_holds_an_assembly_to_its_focus_reach() {
    check_with(
        Options { mode: "play".into(), streaming_target_radius: Some(400.0), ..Options::default() },
        r#"
        local function part(size, at)
            local P = Instance.new("Part")
            P.Anchored = false
            P.CanCollide = false
            P.Size = Vector3.one * size
            P.CFrame = CFrame.new(at)
            P.Parent = workspace
            return P
        end
        local function anchor(at)
            local A = Instance.new("Part")
            A.Anchored = true
            A.CFrame = CFrame.new(at)
            A.Parent = workspace
            return A
        end
        local Player = game:GetService("Players").LocalPlayer
        Player.ReplicationFocus = anchor(Vector3.zero)
        Player:AddReplicationFocus(anchor(Vector3.new(2048, 2048, 2048)))
        local Cage = part(400, Vector3.new(2048, 2048, 2048))
        local Cube = part(300, Vector3.zero)
        local Small = part(4, Vector3.new(300, 0, 0))
        local Beyond = part(4, Vector3.new(390, 0, 0))
        task.wait(1.6)
        local CageDrop = 2048 - Cage.Position.Y
        assert(CageDrop > 5.5 and CageDrop < 6.8, `the cage fell {CageDrop}`)
        assert(Cage.Velocity.Y < -40, "the cage lost its velocity")
        local CubeDrop = -Cube.Position.Y
        assert(CubeDrop > 123 and CubeDrop < 130, `the cube fell {CubeDrop}`)
        assert(-Small.Position.Y > 200, `the small part stopped at {-Small.Position.Y}`)
        assert(Beyond.Position.Y == 0, "a part past the character's reach fell")
        "#,
        2.0,
    );
}
