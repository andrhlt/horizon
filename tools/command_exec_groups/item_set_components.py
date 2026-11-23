GROUP = 'item modify component patches'


def cases(cmd, setup):
    target = 'entity @s weapon.mainhand'
    selected = '/data get entity @s SelectedItem'

    def modify(patch):
        return cmd(f'/item modify {target} {{type:"minecraft:set_components",components:{patch}}}',
                   expect_result=1)

    return [
        *setup('/clear @s'),
        cmd(f'/item replace {target} with minecraft:mud_brick_stairs[break_sound="minecraft:block.stone.break",custom_data={{kept:1}}]'),
        modify('{"minecraft:break_sound":"entity.item.break"}'),
        cmd(f'{selected}.components', expect_result=1),
        cmd(f'/item replace {target} with minecraft:bush[custom_data={{temporary:1}}]'),
        cmd(f'/execute if items {target} *[minecraft:use_effects]', expect_result=1),
        modify('{"!minecraft:use_effects":{},"!minecraft:custom_data":{}}'),
        cmd(f'{selected}.components', expect_result=1),
        cmd(f'/execute if items {target} *[!minecraft:use_effects]', expect_result=1),
        cmd(f'/item replace {target} with minecraft:compass[custom_data={{kept:1}}] 64'),
        modify('{"minecraft:max_stack_size":16,"minecraft:rarity":"rare"}'),
        cmd(f'{selected}.count', expect_result=64),
        cmd(f'{selected}.components', expect_result=1),
        *setup('/clear @s'),
    ]
